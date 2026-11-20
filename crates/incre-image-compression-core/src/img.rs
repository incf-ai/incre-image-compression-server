//! Pictures in memory: decoded sources and outputs, and their linear-light copies for the metric.

use std::io::Cursor;

use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageReader};

use crate::Error;

/// A source's container, from its first bytes (the name's extension can lie).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// JPEG (JFIF or EXIF).
    Jpeg,
    /// PNG.
    Png,
    /// GIF, still or animated.
    Gif,
    /// Windows bitmap.
    Bmp,
    /// WebP.
    Webp,
    /// Anything else.
    Other,
}

impl Format {
    /// The format of `bytes`, from its magic number.
    #[must_use]
    pub fn sniff(bytes: &[u8]) -> Self {
        match bytes {
            [0xFF, 0xD8, 0xFF, ..] => Self::Jpeg,
            [0x89, b'P', b'N', b'G', ..] => Self::Png,
            [b'G', b'I', b'F', b'8', ..] => Self::Gif,
            [b'B', b'M', ..] => Self::Bmp,
            [
                b'R',
                b'I',
                b'F',
                b'F',
                _,
                _,
                _,
                _,
                b'W',
                b'E',
                b'B',
                b'P',
                ..,
            ] => Self::Webp,
            _ => Self::Other,
        }
    }

    /// A lower-case name for reports.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Png => "png",
            Self::Gif => "gif",
            Self::Bmp => "bmp",
            Self::Webp => "webp",
            Self::Other => "other",
        }
    }
}

/// Interleaved samples, rows top to bottom. `channels` is 1 (grey), 2 (grey and alpha), 3 (RGB)
/// or 4 (RGBA); 8-bit samples are stored as 0..=255.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Img {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Samples per pixel.
    pub channels: u8,
    /// 16-bit samples (else 8-bit).
    pub sixteen: bool,
    /// The samples.
    pub data: Vec<u16>,
    /// The embedded ICC profile, if any.
    pub icc: Option<Vec<u8>>,
}

impl Img {
    /// Pixels.
    #[must_use]
    pub fn pixels(&self) -> usize {
        usize::try_from(u64::from(self.width) * u64::from(self.height)).unwrap_or(usize::MAX)
    }

    /// Whether the picture has an alpha channel.
    #[must_use]
    pub fn has_alpha(&self) -> bool {
        self.channels == 2 || self.channels == 4
    }

    /// Whether the picture is grey (one colour channel).
    #[must_use]
    pub fn is_grey(&self) -> bool {
        self.channels <= 2
    }

    /// The largest sample value.
    #[must_use]
    pub fn max_value(&self) -> u16 {
        if self.sixteen { u16::MAX } else { 255 }
    }

    /// The samples as bytes, for 8-bit pictures.
    #[must_use]
    pub fn bytes8(&self) -> Vec<u8> {
        self.data
            .iter()
            .map(|&v| u8::try_from(v).unwrap_or(u8::MAX))
            .collect()
    }

    /// The samples as RGB or RGBA (grey expanded), in their own depth.
    #[must_use]
    pub fn expanded(&self) -> (Vec<u16>, u8) {
        match self.channels {
            1 => (self.data.iter().flat_map(|&v| [v, v, v]).collect(), 3),
            2 => (
                self.data
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .flat_map(|&[v, a]| [v, v, v, a])
                    .collect(),
                4,
            ),
            c => (self.data.clone(), c),
        }
    }

    /// From a decoded `image`, reduced: grey when every pixel is grey, no alpha when every
    /// pixel is opaque, 8-bit unless the source had more.
    #[must_use]
    pub fn from_dynamic(image: DynamicImage, icc: Option<Vec<u8>>) -> Self {
        let (width, height) = (image.width(), image.height());
        let (channels, sixteen, data): (u8, bool, Vec<u16>) = match image {
            DynamicImage::ImageLuma8(b) => {
                (1, false, b.into_raw().into_iter().map(u16::from).collect())
            }
            DynamicImage::ImageLumaA8(b) => {
                (2, false, b.into_raw().into_iter().map(u16::from).collect())
            }
            DynamicImage::ImageRgb8(b) => {
                (3, false, b.into_raw().into_iter().map(u16::from).collect())
            }
            DynamicImage::ImageRgba8(b) => {
                (4, false, b.into_raw().into_iter().map(u16::from).collect())
            }
            DynamicImage::ImageLuma16(b) => (1, true, b.into_raw()),
            DynamicImage::ImageLumaA16(b) => (2, true, b.into_raw()),
            DynamicImage::ImageRgb16(b) => (3, true, b.into_raw()),
            other => (4, true, other.into_rgba16().into_raw()),
        };
        let img = Self {
            width,
            height,
            channels,
            sixteen,
            data,
            icc,
        };
        img.reduced()
    }

    fn reduced(self) -> Self {
        let mut img = self;
        let c = usize::from(img.channels);
        if img.has_alpha()
            && img
                .data
                .chunks_exact(c)
                .all(|p| p.last() == Some(&img.max_value()))
        {
            let keep = c - 1;
            img.data = img
                .data
                .chunks_exact(c)
                .flat_map(|p| p.iter().take(keep).copied())
                .collect();
            img.channels -= 1;
        }
        let c = usize::from(img.channels);
        if c >= 3
            && img
                .data
                .chunks_exact(c)
                .all(|p| matches!(p, [r, g, b, ..] if r == g && g == b))
        {
            img.data = img
                .data
                .chunks_exact(c)
                .flat_map(|p| p.iter().step_by(3).copied())
                .collect();
            img.channels -= 2;
        }
        if img.sixteen && img.data.iter().all(|&v| v % 257 == 0) {
            img.data.iter_mut().for_each(|v| *v /= 257);
            img.sixteen = false;
        }
        img
    }

    /// Whether `other` holds the same pixels, whatever the depth or channel layout each uses.
    #[must_use]
    pub fn same_pixels(&self, other: &Self) -> bool {
        let (a, ca) = self.expanded();
        let (b, cb) = other.expanded();
        if (self.width, self.height) != (other.width, other.height) {
            return false;
        }
        // 8-bit samples widened to 16 bits; a missing alpha is opaque.
        let get = |s: &[u16], i: usize, sixteen: bool| {
            s.get(i)
                .map_or(u16::MAX, |&v| if sixteen { v } else { v * 257 })
        };
        let pa = a.chunks_exact(usize::from(ca));
        let pb = b.chunks_exact(usize::from(cb));
        pa.zip(pb)
            .all(|(p, q)| (0..4).all(|i| get(p, i, self.sixteen) == get(q, i, other.sixteen)))
    }

    /// Linear-light RGB, alpha composited onto `background` (0 black, 1 white), assuming the
    /// samples are sRGB-encoded.
    #[must_use]
    pub fn linear(&self, background: f32) -> Vec<[f32; 3]> {
        let (w, h) = (
            usize::try_from(self.width).unwrap_or(0),
            usize::try_from(self.height).unwrap_or(0),
        );
        self.linear_rect(0, 0, w, h, background)
    }

    /// [`Img::linear`] of the `w`×`h` rectangle at (`x`, `y`), rows top to bottom.
    #[must_use]
    pub fn linear_rect(
        &self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        background: f32,
    ) -> Vec<[f32; 3]> {
        let max = self.max_value();
        let scale = f32::from(max);
        let lut: Vec<f32> = (0..=max)
            .map(|v| srgb_to_linear(f32::from(v) / scale))
            .collect();
        let lin = |v: u16| lut.get(usize::from(v)).copied().unwrap_or(1.0);
        let alpha = |v: u16| f32::from(v) / scale;
        let over = |c: f32, a: f32| c * a + background * (1.0 - a);
        let c = usize::from(self.channels);
        let width = usize::try_from(self.width).unwrap_or(0);
        (y..y + h)
            .flat_map(|r| {
                let start = (r * width + x) * c;
                self.data
                    .get(start..start + w * c)
                    .unwrap_or(&[])
                    .chunks_exact(c)
            })
            .map(|p| match *p {
                [v] => [lin(v); 3],
                [v, a] => [over(lin(v), alpha(a)); 3],
                [r, g, b] => [lin(r), lin(g), lin(b)],
                [r, g, b, a] => {
                    let a = alpha(a);
                    [over(lin(r), a), over(lin(g), a), over(lin(b), a)]
                }
                _ => [0.0; 3],
            })
            .collect()
    }

    /// Distinct colours (alpha included), counted up to `cap`.
    #[must_use]
    pub fn colours(&self, cap: usize) -> usize {
        let mut seen = std::collections::HashSet::new();
        for p in self.data.chunks_exact(usize::from(self.channels)) {
            let key = p.iter().fold(0_u64, |k, &v| (k << 16) | u64::from(v));
            if seen.insert(key) && seen.len() >= cap {
                break;
            }
        }
        seen.len()
    }
}

pub(crate) fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// Whether `bytes` is a GIF with more than one frame.
#[must_use]
pub fn is_animated_gif(bytes: &[u8]) -> bool {
    Format::sniff(bytes) == Format::Gif
        && image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
            .is_ok_and(|d| d.into_frames().take(2).count() > 1)
}

/// Decodes a source (JPEG, PNG, GIF, BMP or WebP), EXIF orientation applied.
///
/// # Errors
///
/// If the format is unknown or the data is corrupt.
pub fn decode(bytes: &[u8]) -> Result<Img, Error> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| Error::new(format!("image: {e}")))?;
    reader.no_limits();
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| Error::new(format!("image: {e}")))?;
    let icc = decoder.icc_profile().ok().flatten();
    let orientation = decoder.orientation().ok();
    let mut image =
        DynamicImage::from_decoder(decoder).map_err(|e| Error::new(format!("image: {e}")))?;
    if let Some(o) = orientation {
        image.apply_orientation(o);
    }
    Ok(Img::from_dynamic(image, icc))
}

/// A source's width and height, from its header.
///
/// # Errors
///
/// If the format is unknown or the header is corrupt.
pub fn dimensions(bytes: &[u8]) -> Result<(u32, u32), Error> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| Error::new(format!("image: {e}")))?
        .into_dimensions()
        .map_err(|e| Error::new(format!("image: {e}")))
}

impl Img {
    /// The size whose short edge is `short`, aspect kept, when the short edge is over
    /// `short × margin`; `None` when the picture is small enough as it is.
    #[must_use]
    pub fn fit(&self, short: u32, margin: f64) -> Option<(u32, u32)> {
        let (w, h) = (self.width, self.height);
        let edge = w.min(h);
        if f64::from(edge) <= f64::from(short) * margin {
            return None;
        }
        // v × short / edge, rounded; it fits a u32 as it is below v.
        let side = |v: u32| {
            let s = (u64::from(v) * u64::from(short) + u64::from(edge) / 2) / u64::from(edge);
            u32::try_from(s).unwrap_or(v).max(1)
        };
        Some(if w <= h {
            (short, side(h))
        } else {
            (side(w), short)
        })
    }

    /// Resized to `w`×`h` with a Lanczos-3 filter on the encoded samples, in the picture's own
    /// depth and channels.
    #[must_use]
    pub fn resized(&self, w: u32, h: u32) -> Self {
        use image::imageops::{FilterType, resize};
        use image::{ImageBuffer, Luma, LumaA, Rgb, Rgba};
        let f = FilterType::Lanczos3;
        let (sw, sh) = (self.width, self.height);
        let data: Vec<u16> = if self.sixteen {
            let d = self.data.clone();
            match self.channels {
                1 => ImageBuffer::<Luma<u16>, _>::from_raw(sw, sh, d)
                    .map(|b| resize(&b, w, h, f).into_raw()),
                2 => ImageBuffer::<LumaA<u16>, _>::from_raw(sw, sh, d)
                    .map(|b| resize(&b, w, h, f).into_raw()),
                3 => ImageBuffer::<Rgb<u16>, _>::from_raw(sw, sh, d)
                    .map(|b| resize(&b, w, h, f).into_raw()),
                _ => ImageBuffer::<Rgba<u16>, _>::from_raw(sw, sh, d)
                    .map(|b| resize(&b, w, h, f).into_raw()),
            }
            .unwrap_or_default()
        } else {
            let d = self.bytes8();
            let out: Option<Vec<u8>> = match self.channels {
                1 => ImageBuffer::<Luma<u8>, _>::from_raw(sw, sh, d)
                    .map(|b| resize(&b, w, h, f).into_raw()),
                2 => ImageBuffer::<LumaA<u8>, _>::from_raw(sw, sh, d)
                    .map(|b| resize(&b, w, h, f).into_raw()),
                3 => ImageBuffer::<Rgb<u8>, _>::from_raw(sw, sh, d)
                    .map(|b| resize(&b, w, h, f).into_raw()),
                _ => ImageBuffer::<Rgba<u8>, _>::from_raw(sw, sh, d)
                    .map(|b| resize(&b, w, h, f).into_raw()),
            };
            out.unwrap_or_default().into_iter().map(u16::from).collect()
        };
        Self {
            width: w,
            height: h,
            channels: self.channels,
            sixteen: self.sixteen,
            data,
            icc: self.icc.clone(),
        }
    }
}

#[cfg(test)]
mod fit_tests {
    use super::*;

    fn blank(width: u32, height: u32) -> Img {
        Img {
            width,
            height,
            channels: 3,
            sixteen: false,
            data: vec![0; usize::try_from(width * height * 3).unwrap_or(0)],
            icc: None,
        }
    }

    #[test]
    #[expect(clippy::disallowed_macros, reason = "tests assert")]
    fn fits_the_short_edge_past_the_margin() {
        let m = 4.0 / 3.0;
        assert_eq!(blank(2730, 4000).fit(2048, m), None);
        assert_eq!(blank(4096, 3000).fit(2048, m), Some((2796, 2048)));
        assert_eq!(blank(3000, 6000).fit(2048, m), Some((2048, 4096)));
        let r = blank(30, 20).resized(15, 10);
        assert_eq!((r.width, r.height, r.data.len()), (15, 10, 15 * 10 * 3));
    }
}
