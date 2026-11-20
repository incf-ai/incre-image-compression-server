//! JPEG XL through the reference libjxl: lossless (modular), lossy (VarDCT, XYB), JPEG
//! transcoding (bit-exact, the original JPEG reconstructible) and decoding.

use std::ffi::c_void;

use gamut_jxl_sys::decode as dec;
use gamut_jxl_sys::encode as enc;
use gamut_jxl_sys::types::{
    JxlBasicInfo, JxlBool, JxlColorEncoding, JxlDataType, JxlEndianness, JxlPixelFormat,
    JxlTransferFunction,
};

use crate::Error;
use crate::img::Img;

/// How to code the pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    /// Modular, every sample kept.
    Lossless,
    /// VarDCT at this Butteraugli distance (1.0 is visually lossless).
    Lossy(f32),
}

/// libjxl's version, `major.minor.patch`.
#[must_use]
pub fn version() -> String {
    // SAFETY: takes nothing, returns a number.
    let v = unsafe { enc::JxlEncoderVersion() };
    format!("libjxl {}.{}.{}", v / 1_000_000, v / 1000 % 1000, v % 1000)
}

struct Encoder(*mut enc::JxlEncoder);
impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: from JxlEncoderCreate, destroyed once (with its frame settings).
        unsafe { enc::JxlEncoderDestroy(self.0) }
    }
}
struct Decoder(*mut dec::JxlDecoder);
impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: from JxlDecoderCreate, destroyed once.
        unsafe { dec::JxlDecoderDestroy(self.0) }
    }
}

fn ok(s: enc::JxlEncoderStatus, e: &Encoder, what: &str) -> Result<(), Error> {
    if s == enc::JxlEncoderStatus::SUCCESS {
        return Ok(());
    }
    // SAFETY: a valid encoder.
    let code = unsafe { enc::JxlEncoderGetError(e.0) };
    Err(Error::new(format!("jxl: {what}: error {}", code.0)))
}

fn jbool(b: bool) -> JxlBool {
    if b { JxlBool::TRUE } else { JxlBool::FALSE }
}

fn new_encoder(effort: i64) -> Result<(Encoder, *mut enc::JxlEncoderFrameSettings), Error> {
    // SAFETY: null memory manager means libjxl's own allocator; null is checked.
    let e = Encoder(unsafe { enc::JxlEncoderCreate(std::ptr::null()) });
    if e.0.is_null() {
        return Err(Error::new("jxl: JxlEncoderCreate failed"));
    }
    // SAFETY: a valid encoder; the settings belong to it.
    let fs = unsafe { enc::JxlEncoderFrameSettingsCreate(e.0, std::ptr::null()) };
    if fs.is_null() {
        return Err(Error::new("jxl: JxlEncoderFrameSettingsCreate failed"));
    }
    // SAFETY: valid frame settings.
    let s = unsafe {
        enc::JxlEncoderFrameSettingsSetOption(fs, enc::JxlEncoderFrameSettingId::EFFORT, effort)
    };
    ok(s, &e, "effort")?;
    Ok((e, fs))
}

fn finish(e: &Encoder) -> Result<Vec<u8>, Error> {
    // SAFETY: a valid encoder.
    unsafe { enc::JxlEncoderCloseInput(e.0) };
    let mut out = vec![0_u8; 1 << 16];
    let mut written = 0;
    loop {
        let rest = out
            .get_mut(written..)
            .ok_or_else(|| Error::new("jxl: output overflow"))?;
        let mut next = rest.as_mut_ptr();
        let mut avail = rest.len();
        // SAFETY: `next` points at `avail` writable bytes of `out`.
        let s = unsafe { enc::JxlEncoderProcessOutput(e.0, &raw mut next, &raw mut avail) };
        written = out.len() - avail;
        if s == enc::JxlEncoderStatus::NEED_MORE_OUTPUT {
            out.resize(out.len() * 2, 0);
            continue;
        }
        ok(s, e, "encode")?;
        out.truncate(written);
        return Ok(out);
    }
}

/// Encodes decoded pixels, one thread.
///
/// # Errors
///
/// If libjxl refuses the picture or the settings.
pub fn encode(img: &Img, mode: Mode, effort: i64) -> Result<Vec<u8>, Error> {
    // libjxl wants the profile's colour space to match the channels: a grey picture carrying an
    // RGB profile (common in JPEGs) is encoded as RGB.
    let rgb_profile = img
        .icc
        .as_deref()
        .is_some_and(|p| p.get(16..20) != Some(b"GRAY".as_slice()));
    let expanded;
    let img = if img.is_grey() && rgb_profile {
        let (data, channels) = img.expanded();
        expanded = Img {
            data,
            channels,
            ..img.clone()
        };
        &expanded
    } else {
        img
    };
    let (e, fs) = new_encoder(effort)?;
    let lossless = mode == Mode::Lossless;
    let bits: u32 = if img.sixteen { 16 } else { 8 };
    // SAFETY: JxlBasicInfo is plain data; JxlEncoderInitBasicInfo fills it.
    let mut info: JxlBasicInfo = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is valid for writes.
    unsafe { enc::JxlEncoderInitBasicInfo(&raw mut info) };
    info.xsize = img.width;
    info.ysize = img.height;
    info.bits_per_sample = bits;
    info.num_color_channels = if img.is_grey() { 1 } else { 3 };
    if img.has_alpha() {
        info.num_extra_channels = 1;
        info.alpha_bits = bits;
    }
    info.uses_original_profile = jbool(lossless);
    ok(
        // SAFETY: valid encoder and info.
        unsafe { enc::JxlEncoderSetBasicInfo(e.0, &raw const info) },
        &e,
        "basic info",
    )?;
    if let Some(icc) = &img.icc {
        ok(
            // SAFETY: libjxl copies the profile.
            unsafe { enc::JxlEncoderSetICCProfile(e.0, icc.as_ptr(), icc.len()) },
            &e,
            "ICC",
        )?;
    } else {
        // SAFETY: plain data, filled by JxlColorEncodingSetToSRGB.
        let mut c: JxlColorEncoding = unsafe { std::mem::zeroed() };
        // SAFETY: `c` is valid for writes; the encoder copies it.
        let s = unsafe {
            enc::JxlColorEncodingSetToSRGB(&raw mut c, jbool(img.is_grey()));
            enc::JxlEncoderSetColorEncoding(e.0, &raw const c)
        };
        ok(s, &e, "colour encoding")?;
    }
    match mode {
        Mode::Lossless => ok(
            // SAFETY: valid frame settings.
            unsafe { enc::JxlEncoderSetFrameLossless(fs, JxlBool::TRUE) },
            &e,
            "lossless",
        )?,
        Mode::Lossy(d) => ok(
            // SAFETY: valid frame settings.
            unsafe { enc::JxlEncoderSetFrameDistance(fs, d) },
            &e,
            "distance",
        )?,
    }
    let format = JxlPixelFormat {
        num_channels: u32::from(img.channels),
        data_type: if img.sixteen {
            JxlDataType::UINT16
        } else {
            JxlDataType::UINT8
        },
        endianness: JxlEndianness::NATIVE,
        align: 0,
    };
    let bytes: Vec<u8> = if img.sixteen {
        img.data.iter().flat_map(|v| v.to_ne_bytes()).collect()
    } else {
        img.bytes8()
    };
    // SAFETY: `bytes` holds width*height*channels samples in `format`; libjxl copies them.
    let s = unsafe {
        enc::JxlEncoderAddImageFrame(
            fs,
            &raw const format,
            bytes.as_ptr().cast::<c_void>(),
            bytes.len(),
        )
    };
    ok(s, &e, "frame")?;
    finish(&e)
}

/// Transcodes a JPEG losslessly, keeping what's needed to give back the exact file.
///
/// # Errors
///
/// If libjxl can't represent the JPEG (some progressive or odd files).
pub fn encode_jpeg(jpeg: &[u8], effort: i64) -> Result<Vec<u8>, Error> {
    let (e, fs) = new_encoder(effort)?;
    // SAFETY: a valid encoder, before any frame.
    unsafe {
        ok(
            enc::JxlEncoderUseContainer(e.0, JxlBool::TRUE),
            &e,
            "container",
        )?;
        ok(
            enc::JxlEncoderStoreJPEGMetadata(e.0, JxlBool::TRUE),
            &e,
            "JPEG metadata",
        )?;
    }
    ok(
        // SAFETY: libjxl reads (copies) `jpeg`.
        unsafe { enc::JxlEncoderAddJPEGFrame(fs, jpeg.as_ptr(), jpeg.len()) },
        &e,
        "JPEG frame",
    )?;
    finish(&e)
}

/// What [`decode_any`] gives back.
struct Decoded {
    img: Img,
    jpeg: Option<Vec<u8>>,
}

fn decode_any(bytes: &[u8], want_jpeg: bool) -> Result<Decoded, Error> {
    // SAFETY: libjxl's allocator; null is checked.
    let d = Decoder(unsafe { dec::JxlDecoderCreate(std::ptr::null()) });
    if d.0.is_null() {
        return Err(Error::new("jxl: JxlDecoderCreate failed"));
    }
    let mut events = dec::JxlDecoderStatus::BASIC_INFO.0
        | dec::JxlDecoderStatus::COLOR_ENCODING.0
        | dec::JxlDecoderStatus::FULL_IMAGE.0;
    if want_jpeg {
        events |= dec::JxlDecoderStatus::JPEG_RECONSTRUCTION.0;
    }
    let fail = |what: &str| Error::new(format!("jxl: decode: {what}"));
    // SAFETY: a valid decoder; `bytes` outlives it.
    unsafe {
        if dec::JxlDecoderSubscribeEvents(d.0, events) != dec::JxlDecoderStatus::SUCCESS
            || dec::JxlDecoderSetInput(d.0, bytes.as_ptr(), bytes.len())
                != dec::JxlDecoderStatus::SUCCESS
        {
            return Err(fail("setup"));
        }
        dec::JxlDecoderCloseInput(d.0);
    }
    // SAFETY: plain data.
    let mut info: JxlBasicInfo = unsafe { std::mem::zeroed() };
    let mut channels: u8 = 3;
    let mut pixels: Vec<u16> = Vec::new();
    let mut jpeg: Vec<u8> = Vec::new();
    let mut jpeg_done = false;
    loop {
        // SAFETY: a valid decoder; any buffer handed over below lives until the loop ends.
        let s = unsafe { dec::JxlDecoderProcessInput(d.0) };
        match s {
            dec::JxlDecoderStatus::BASIC_INFO => {
                // SAFETY: basic info is available at this event.
                if unsafe { dec::JxlDecoderGetBasicInfo(d.0, &raw mut info) }
                    != dec::JxlDecoderStatus::SUCCESS
                {
                    return Err(fail("basic info"));
                }
                let colour = u8::try_from(info.num_color_channels).map_err(|_| fail("channels"))?;
                channels = colour + u8::from(info.alpha_bits > 0);
            }
            dec::JxlDecoderStatus::COLOR_ENCODING => {
                // SAFETY: plain data; the colour encoding is available at this event.
                let mut c: JxlColorEncoding = unsafe { std::mem::zeroed() };
                // SAFETY: `d.0` is a live decoder and `c` is a valid out-pointer.
                let st = unsafe {
                    dec::JxlDecoderGetColorAsEncodedProfile(
                        d.0,
                        dec::JxlColorProfileTarget::DATA,
                        &raw mut c,
                    )
                };
                if st == dec::JxlDecoderStatus::SUCCESS
                    && c.transfer_function == JxlTransferFunction::LINEAR
                {
                    return Err(fail("pixels come out linear, not sRGB"));
                }
            }
            dec::JxlDecoderStatus::JPEG_RECONSTRUCTION => {
                jpeg.resize(1 << 20, 0);
                // SAFETY: `jpeg` is writable and outlives the decoder's use of it.
                if unsafe { dec::JxlDecoderSetJPEGBuffer(d.0, jpeg.as_mut_ptr(), jpeg.len()) }
                    != dec::JxlDecoderStatus::SUCCESS
                {
                    return Err(fail("JPEG buffer"));
                }
                jpeg_done = true;
            }
            dec::JxlDecoderStatus::JPEG_NEED_MORE_OUTPUT => {
                // SAFETY: a JPEG buffer is set; this returns the unused bytes at its end.
                let unused = unsafe { dec::JxlDecoderReleaseJPEGBuffer(d.0) };
                let used = jpeg.len() - unused;
                jpeg.resize(jpeg.len() * 2, 0);
                let rest = jpeg.get_mut(used..).ok_or_else(|| fail("JPEG buffer"))?;
                // SAFETY: as above, the rest of the grown buffer.
                if unsafe { dec::JxlDecoderSetJPEGBuffer(d.0, rest.as_mut_ptr(), rest.len()) }
                    != dec::JxlDecoderStatus::SUCCESS
                {
                    return Err(fail("JPEG buffer"));
                }
            }
            dec::JxlDecoderStatus::NEED_IMAGE_OUT_BUFFER => {
                let format = pixel_format(channels);
                let mut size = 0;
                // SAFETY: valid decoder and format.
                if unsafe {
                    dec::JxlDecoderImageOutBufferSize(d.0, &raw const format, &raw mut size)
                } != dec::JxlDecoderStatus::SUCCESS
                {
                    return Err(fail("buffer size"));
                }
                pixels = vec![0; size / 2];
                // SAFETY: `pixels` has `size` bytes and outlives the decoder's use of it.
                if unsafe {
                    dec::JxlDecoderSetImageOutBuffer(
                        d.0,
                        &raw const format,
                        pixels.as_mut_ptr().cast::<c_void>(),
                        size,
                    )
                } != dec::JxlDecoderStatus::SUCCESS
                {
                    return Err(fail("image buffer"));
                }
            }
            dec::JxlDecoderStatus::FULL_IMAGE => {}
            dec::JxlDecoderStatus::SUCCESS => break,
            other => return Err(fail(&format!("status {}", other.0))),
        }
    }
    let jpeg = if jpeg_done {
        // SAFETY: a JPEG buffer is set; returns the unused bytes at its end.
        let unused = unsafe { dec::JxlDecoderReleaseJPEGBuffer(d.0) };
        let total = jpeg.len();
        jpeg.truncate(total - unused);
        Some(jpeg)
    } else {
        None
    };
    let img = Img {
        width: info.xsize,
        height: info.ysize,
        channels,
        sixteen: true,
        data: pixels,
        icc: None,
    };
    Ok(Decoded { img, jpeg })
}

fn pixel_format(channels: u8) -> JxlPixelFormat {
    JxlPixelFormat {
        num_channels: u32::from(channels),
        data_type: JxlDataType::UINT16,
        endianness: JxlEndianness::NATIVE,
        align: 0,
    }
}

/// Decodes to 16-bit samples (grey, grey and alpha, RGB or RGBA as coded), sRGB-encoded.
///
/// # Errors
///
/// If the data isn't a decodable JPEG XL, or it would decode to linear light.
pub fn decode(bytes: &[u8]) -> Result<Img, Error> {
    decode_any(bytes, false).map(|d| d.img)
}

/// The original JPEG a transcoded file was made from.
///
/// # Errors
///
/// If the data isn't decodable or holds no JPEG reconstruction data.
pub fn reconstruct_jpeg(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    decode_any(bytes, true)?
        .jpeg
        .ok_or_else(|| Error::new("jxl: no JPEG reconstruction data"))
}
