//! AVIF through libavif: libaom encodes (4:4:4, or 4:0:0 for grey, 10-bit), dav1d decodes.

use std::ffi::{CStr, CString, c_char};

use libavif_sys as ffi;

use crate::Error;
use crate::img::Img;

/// One encoder setting: speed, quality and libaom options (`tune`, `tune-content`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// libaom speed, 0 (slowest) to 10.
    pub speed: i32,
    /// libavif quality, 0 to 100.
    pub quality: i32,
    /// libaom's options by their aomenc names.
    pub options: Vec<(String, String)>,
}

/// libavif's and its codecs' versions.
#[must_use]
pub fn versions() -> String {
    let mut buf: [c_char; 256] = [0; 256];
    // SAFETY: libavif writes at most 256 bytes, NUL-terminated, into the buffer.
    unsafe { ffi::avifCodecVersions(buf.as_mut_ptr()) };
    // SAFETY: avifVersion returns a static NUL-terminated string; `buf` was NUL-terminated above.
    let (lib, codecs) = unsafe {
        (
            CStr::from_ptr(ffi::avifVersion()),
            CStr::from_ptr(buf.as_ptr()),
        )
    };
    format!(
        "libavif {} ({})",
        lib.to_string_lossy(),
        codecs.to_string_lossy()
    )
}

fn check(r: ffi::avifResult, what: &str) -> Result<(), Error> {
    if r == ffi::AVIF_RESULT_OK {
        return Ok(());
    }
    // SAFETY: avifResultToString returns a static NUL-terminated string for any value.
    let s = unsafe { CStr::from_ptr(ffi::avifResultToString(r)) };
    Err(Error::new(format!("avif: {what}: {}", s.to_string_lossy())))
}

struct Image(*mut ffi::avifImage);
impl Drop for Image {
    fn drop(&mut self) {
        // SAFETY: the pointer came from avifImageCreate*/ and is destroyed once.
        unsafe { ffi::avifImageDestroy(self.0) }
    }
}
struct Encoder(*mut ffi::avifEncoder);
impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: the pointer came from avifEncoderCreate and is destroyed once.
        unsafe { ffi::avifEncoderDestroy(self.0) }
    }
}
struct Decoder(*mut ffi::avifDecoder);
impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: the pointer came from avifDecoderCreate and is destroyed once.
        unsafe { ffi::avifDecoderDestroy(self.0) }
    }
}
struct Output(ffi::avifRWData);
impl Drop for Output {
    fn drop(&mut self) {
        // SAFETY: libavif allocated the data (or left it null), freed once here.
        unsafe { ffi::avifRWDataFree(&raw mut self.0) }
    }
}

/// Encodes `img` with libaom, one thread.
///
/// # Errors
///
/// If libavif or libaom refuses the picture or a setting.
pub fn encode(img: &Img, s: &Settings) -> Result<Vec<u8>, Error> {
    let format = if img.is_grey() {
        ffi::AVIF_PIXEL_FORMAT_YUV400
    } else {
        ffi::AVIF_PIXEL_FORMAT_YUV444
    };
    // SAFETY: plain constructor; null is checked.
    let image = Image(unsafe { ffi::avifImageCreate(img.width, img.height, 10, format) });
    if image.0.is_null() {
        return Err(Error::new("avif: avifImageCreate failed"));
    }
    // SAFETY: `image.0` is a valid, exclusively owned avifImage.
    unsafe {
        (*image.0).yuvRange = ffi::AVIF_RANGE_FULL;
        (*image.0).colorPrimaries = 1; // BT.709 (sRGB)
        (*image.0).transferCharacteristics = 13; // sRGB
        (*image.0).matrixCoefficients = 6; // BT.601
    }
    if let Some(icc) = &img.icc {
        check(
            // SAFETY: libavif copies the profile.
            unsafe { ffi::avifImageSetProfileICC(image.0, icc.as_ptr(), icc.len()) },
            "ICC",
        )?;
    }
    let (samples, channels) = img.expanded();
    let mut bytes: Vec<u8> = if img.sixteen {
        samples.iter().flat_map(|v| v.to_ne_bytes()).collect()
    } else {
        samples
            .iter()
            .map(|&v| u8::try_from(v).unwrap_or(u8::MAX))
            .collect()
    };
    let mut rgb = ffi::avifRGBImage::default();
    // SAFETY: fills `rgb` from the image's size and depth.
    unsafe { ffi::avifRGBImageSetDefaults(&raw mut rgb, image.0) };
    rgb.depth = if img.sixteen { 16 } else { 8 };
    rgb.format = if channels == 4 {
        ffi::AVIF_RGB_FORMAT_RGBA
    } else {
        ffi::AVIF_RGB_FORMAT_RGB
    };
    rgb.maxThreads = 1;
    rgb.pixels = bytes.as_mut_ptr();
    rgb.rowBytes = img.width * u32::from(channels) * if img.sixteen { 2 } else { 1 };
    check(
        // SAFETY: `rgb.pixels` points at width*height*channels samples of `rgb.depth` that outlive the call.
        unsafe { ffi::avifImageRGBToYUV(image.0, &raw const rgb) },
        "RGB to YUV",
    )?;

    // SAFETY: plain constructor; null is checked.
    let encoder = Encoder(unsafe { ffi::avifEncoderCreate() });
    if encoder.0.is_null() {
        return Err(Error::new("avif: avifEncoderCreate failed"));
    }
    // SAFETY: `encoder.0` is a valid, exclusively owned avifEncoder.
    unsafe {
        (*encoder.0).codecChoice = ffi::AVIF_CODEC_CHOICE_AOM;
        (*encoder.0).maxThreads = 1;
        (*encoder.0).speed = s.speed;
        (*encoder.0).quality = s.quality;
        (*encoder.0).qualityAlpha = s.quality;
    }
    for (k, v) in &s.options {
        let (k, v) = (cstring(k)?, cstring(v)?);
        check(
            // SAFETY: libavif copies both strings.
            unsafe { ffi::avifEncoderSetCodecSpecificOption(encoder.0, k.as_ptr(), v.as_ptr()) },
            "option",
        )?;
    }
    let mut out = Output(ffi::avifRWData::default());
    check(
        // SAFETY: valid encoder and image; libavif allocates `out`.
        unsafe { ffi::avifEncoderWrite(encoder.0, image.0, &raw mut out.0) },
        "encode",
    )?;
    if out.0.data.is_null() {
        return Err(Error::new("avif: empty output"));
    }
    // SAFETY: libavif wrote `size` bytes at `data`.
    Ok(unsafe { std::slice::from_raw_parts(out.0.data, out.0.size) }.to_vec())
}

fn cstring(s: &str) -> Result<CString, Error> {
    CString::new(s).map_err(|_| Error::new(format!("avif: NUL in option {s:?}")))
}

/// Decodes an AVIF with dav1d, one thread, to 16-bit RGB or RGBA.
///
/// # Errors
///
/// If the data isn't a decodable AVIF.
pub fn decode(bytes: &[u8]) -> Result<Img, Error> {
    // SAFETY: plain constructor; null is checked.
    let decoder = Decoder(unsafe { ffi::avifDecoderCreate() });
    // SAFETY: plain constructor; null is checked.
    let image = Image(unsafe { ffi::avifImageCreateEmpty() });
    if decoder.0.is_null() || image.0.is_null() {
        return Err(Error::new("avif: decoder create failed"));
    }
    // SAFETY: `decoder.0` is valid and exclusively owned.
    unsafe {
        (*decoder.0).codecChoice = ffi::AVIF_CODEC_CHOICE_DAV1D;
        (*decoder.0).maxThreads = 1;
    }
    check(
        // SAFETY: `bytes` outlives the call; the decoder fills `image`.
        unsafe { ffi::avifDecoderReadMemory(decoder.0, image.0, bytes.as_ptr(), bytes.len()) },
        "decode",
    )?;
    let mut rgb = ffi::avifRGBImage::default();
    // SAFETY: `image.0` holds the decoded picture.
    let alpha = unsafe {
        ffi::avifRGBImageSetDefaults(&raw mut rgb, image.0);
        !(*image.0).alphaPlane.is_null()
    };
    rgb.depth = 16;
    rgb.format = if alpha {
        ffi::AVIF_RGB_FORMAT_RGBA
    } else {
        ffi::AVIF_RGB_FORMAT_RGB
    };
    rgb.maxThreads = 1;
    check(
        // SAFETY: allocates pixels for `rgb`'s size, freed below.
        unsafe { ffi::avifRGBImageAllocatePixels(&raw mut rgb) },
        "allocate",
    )?;
    // SAFETY: `rgb` has pixels for the image's size.
    let converted = check(
        unsafe { ffi::avifImageYUVToRGB(image.0, &raw mut rgb) },
        "YUV to RGB",
    );
    let channels: u8 = if alpha { 4 } else { 3 };
    let row = usize::try_from(rgb.width).unwrap_or(0) * usize::from(channels);
    let rows = usize::try_from(rgb.height).unwrap_or(0);
    let stride = usize::try_from(rgb.rowBytes).unwrap_or(0);
    let mut data = Vec::with_capacity(row * rows);
    if converted.is_ok() {
        // SAFETY: libavif allocated `rowBytes * height` bytes at `pixels`.
        let all = unsafe { std::slice::from_raw_parts(rgb.pixels, stride * rows) };
        for line in all.chunks_exact(stride.max(1)) {
            data.extend(
                line.as_chunks::<2>()
                    .0
                    .iter()
                    .take(row)
                    .map(|&b| u16::from_ne_bytes(b)),
            );
        }
    }
    // SAFETY: frees what avifRGBImageAllocatePixels allocated.
    unsafe { ffi::avifRGBImageFreePixels(&raw mut rgb) };
    converted?;
    Ok(Img {
        width: rgb.width,
        height: rgb.height,
        channels,
        sixteen: true,
        data,
        icc: None,
    })
}
