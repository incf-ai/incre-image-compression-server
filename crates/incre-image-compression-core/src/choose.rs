//! One picture's recompression, as the benchmark settled it: libaom AVIF at speed 6 searched for
//! the lowest quality that passes the gates, against a lossless JPEG XL floor (a JPEG's exact
//! transcode, or the pixels of a picture with few colours), the source kept when nothing is
//! smaller. When libaom refuses a picture (it fails on the very largest), JPEG XL's lossy and
//! lossless modes stand in.

use std::io;
use std::time::Instant;

use crate::img::{self, Format, Img};
use crate::metric::{self, Score};
use crate::{Error, avif, jxl};
pub use incre_image_compression_proto::{Config, Gates};

/// Scores a decoded picture against its source, per tile.
pub trait Metric {
    /// The metric's name, for the records.
    fn name(&self) -> &'static str;

    /// Scores `decoded` against `source` on linear light composited on `background`, as
    /// [`metric::score`] does.
    ///
    /// # Errors
    ///
    /// If the sizes differ, or the metric fails.
    fn score(
        &self,
        source: &Img,
        decoded: &Img,
        tile: usize,
        background: f32,
    ) -> Result<Score, Error>;
}

/// [`metric::score`]: the `ssimulacra2` crate on the CPU.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cpu;

impl Metric for Cpu {
    fn name(&self) -> &'static str {
        "ssimulacra2 crate (cpu)"
    }

    fn score(
        &self,
        source: &Img,
        decoded: &Img,
        tile: usize,
        background: f32,
    ) -> Result<Score, Error> {
        metric::score(source, decoded, tile, background)
    }
}

/// How a picture is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// The source's own bytes.
    Source,
    /// A JPEG's lossless JPEG XL transcode (the JPEG rebuilds bit for bit).
    JxlJpeg,
    /// Lossless JPEG XL of the pixels.
    JxlLossless,
    /// Lossy JPEG XL (VarDCT) at a Butteraugli distance.
    JxlLossy,
    /// AVIF (libaom) at a libavif quality.
    Avif,
}

impl Codec {
    /// The codec's name in the index.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::JxlJpeg => "jxl-jpeg",
            Self::JxlLossless => "jxl-lossless",
            Self::JxlLossy => "jxl-lossy",
            Self::Avif => "avif",
        }
    }

    /// The stored file's extension, or `None` for the source's own.
    #[must_use]
    pub fn ext(self) -> Option<&'static str> {
        match self {
            Self::Source => None,
            Self::JxlJpeg | Self::JxlLossless | Self::JxlLossy => Some("jxl"),
            Self::Avif => Some("avif"),
        }
    }
}

/// What a picture became.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    /// The codec.
    pub codec: Codec,
    /// The quality or distance, for the lossy codecs.
    pub param: Option<f64>,
    /// The stored bytes.
    pub bytes: Vec<u8>,
    /// The result's score, for the lossy codecs.
    pub score: Option<Score>,
    /// Encodes tried, all candidates.
    pub probes: usize,
    /// Why the source was kept or a fallback taken, if so.
    pub note: Option<String>,
    /// Decoded size, if the source decoded.
    pub size: Option<(u32, u32)>,
    /// The stored size, when the picture was resized.
    pub resized: Option<(u32, u32)>,
}

/// This thread's CPU time, in seconds.
#[must_use]
pub fn thread_cpu() -> f64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `t` is a valid timespec for clock_gettime to fill.
    let r = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut t) };
    if r == 0 {
        let s = u32::try_from(t.tv_sec).map_or(0.0, f64::from);
        let n = i32::try_from(t.tv_nsec).map_or(0.0, f64::from);
        s + n * 1e-9
    } else {
        0.0
    }
}

/// A wall clock and this thread's CPU clock, started together.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    wall: Instant,
    cpu: f64,
}

impl Clock {
    /// Starts both clocks.
    #[must_use]
    pub fn start() -> Self {
        Self {
            wall: Instant::now(),
            cpu: thread_cpu(),
        }
    }

    /// Wall and thread-CPU milliseconds since the start.
    #[must_use]
    pub fn ms(&self) -> (f64, f64) {
        (
            self.wall.elapsed().as_secs_f64() * 1000.0,
            (thread_cpu() - self.cpu) * 1000.0,
        )
    }
}

/// Pins this thread to `cpu` (below 1024).
///
/// # Errors
///
/// If `cpu` is out of range or the kernel refuses.
pub fn pin(cpu: usize) -> Result<(), Error> {
    if cpu >= 1024 {
        return Err(Error::new(format!("CPU {cpu}: out of range")));
    }
    // SAFETY: cpu_set_t is plain data and all-zero is the empty set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: `set` is a valid cpu_set_t and `cpu` < 1024, inside it.
    unsafe { libc::CPU_SET(cpu, &mut set) };
    // SAFETY: pid 0 is this thread; `set` outlives the call and its size is passed with it.
    let r = unsafe {
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &raw const set)
    };
    if r == 0 {
        Ok(())
    } else {
        Err(Error::new(format!(
            "pin to CPU {cpu}: {}",
            io::Error::last_os_error()
        )))
    }
}

/// Scores `decoded` against `source`; a picture with alpha takes the worse of its scores on
/// black and on white.
///
/// # Errors
///
/// If the sizes differ.
pub fn score(
    metric: &dyn Metric,
    source: &Img,
    decoded: &Img,
    tile: usize,
) -> Result<Score, Error> {
    let black = metric.score(source, decoded, tile, 0.0)?;
    if !source.has_alpha() {
        return Ok(black);
    }
    let white = metric.score(source, decoded, tile, 1.0)?;
    let worst = if white.worst.score < black.worst.score {
        white.worst
    } else {
        black.worst
    };
    Ok(Score {
        mean: black.mean.min(white.mean),
        worst,
    })
}

/// Searches `grid` (best quality first) for the last setting whose probe passes, assuming the
/// passing ones are a prefix; starts at `start`, then bisects. Returns the setting and its probe,
/// and the probes made.
///
/// # Errors
///
/// The first error `probe` returns.
pub fn search<T: Copy, R>(
    grid: &[T],
    start: usize,
    mut probe: impl FnMut(T) -> Result<(bool, R), Error>,
) -> Result<(Option<(T, R)>, usize), Error> {
    let mut lo: Option<usize> = None;
    let mut hi = grid.len();
    let mut best = None;
    let mut i = start.min(grid.len().saturating_sub(1));
    let mut probes = 0;
    while let Some(&p) = grid.get(i) {
        let (pass, r) = probe(p)?;
        probes += 1;
        if pass {
            lo = Some(i);
            best = Some((p, r));
        } else {
            hi = i;
        }
        let from = lo.map_or(0, |l| l + 1);
        if from >= hi {
            break;
        }
        i = from + (hi - from) / 2;
    }
    Ok((best, probes))
}

/// A grid index as an `f64`.
fn index(i: usize) -> f64 {
    u32::try_from(i).map_or(f64::MAX, f64::from)
}

/// [`search`] for probes that say by how much they pass (a margin, negative when they fail): the
/// next probe goes where the margin is guessed to cross zero, on the line through the closest
/// pass and fail, or from the last probe at `step` per grid step (negative) until both are seen.
/// On the sweep's 60 pictures this takes 3.6 encodes where bisection takes 4.9, with the same
/// results. Returns the setting and its probe, and the probes made.
///
/// # Errors
///
/// The first error `probe` returns.
pub fn seek<T: Copy, R>(
    grid: &[T],
    start: usize,
    step: f64,
    stop: f64,
    mut probe: impl FnMut(T) -> Result<(f64, R), Error>,
) -> Result<(Option<(T, R)>, usize), Error> {
    let mut lo: Option<(usize, f64)> = None;
    let mut hi: Option<(usize, f64)> = None;
    let mut best = None;
    let mut i = start.min(grid.len().saturating_sub(1));
    let mut probes = 0;
    while let Some(&p) = grid.get(i) {
        let (margin, r) = probe(p)?;
        probes += 1;
        if margin >= 0.0 {
            lo = Some((i, margin));
            best = Some((p, r));
            if margin < stop {
                break;
            }
        } else {
            hi = Some((i, margin));
        }
        let from = lo.map_or(0, |l| l.0 + 1);
        let end = hi.map_or(grid.len(), |h| h.0);
        if from >= end {
            break;
        }
        let cross = match (lo, hi) {
            (Some((a, ma)), Some((b, mb))) => index(a) + ma / (ma - mb) * (index(b) - index(a)),
            _ => index(i) - margin / step,
        };
        // The last setting guessed to pass, inside what is still open.
        i = (from..end)
            .take_while(|&k| index(k) <= cross)
            .last()
            .unwrap_or(from);
    }
    Ok((best, probes))
}

/// A passing lossy encode.
struct Lossy {
    bytes: Vec<u8>,
    score: Score,
}

/// Encodes pictures with a [`Config`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Chooser(pub Config);

impl std::ops::Deref for Chooser {
    type Target = Config;

    fn deref(&self) -> &Config {
        &self.0
    }
}

impl Chooser {
    /// The encode's margin over the gates (the closer of the two; negative when it fails).
    fn gate(
        &self,
        metric: &dyn Metric,
        source: &Img,
        bytes: Vec<u8>,
        decoded: &Img,
    ) -> Result<(f64, Lossy), Error> {
        let s = score(metric, source, decoded, self.gates.tile)?;
        let margin = (s.mean - self.gates.mean).min(s.worst.score - self.gates.worst);
        Ok((margin, Lossy { bytes, score: s }))
    }

    /// The quality the AVIF search starts at for a source file of `bytes` bytes, `pixels`
    /// pixels when decoded, JPEG or not.
    #[must_use]
    pub fn quality_start(&self, bytes: usize, pixels: f64, jpeg: bool) -> i32 {
        let bits = u32::try_from(bytes).map_or(f64::MAX, f64::from) * 8.0 / pixels.max(1.0);
        let starts = if jpeg {
            &self.quality_starts_jpeg
        } else {
            &self.quality_starts_other
        };
        starts
            .iter()
            .find(|s| bits <= s.0)
            .or(starts.last())
            .map_or(78, |s| s.1)
    }

    fn avif(
        &self,
        metric: &dyn Metric,
        source: &Img,
        start: i32,
    ) -> Result<(Option<(i32, Lossy)>, usize), Error> {
        // The nearest quality on the grid.
        let start = self
            .qualities
            .iter()
            .enumerate()
            .min_by_key(|(_, q)| (*q - start).abs())
            .map_or(0, |(i, _)| i);
        seek(
            &self.qualities,
            start,
            self.margin_step,
            self.stop_margin,
            |quality| {
                let settings = avif::Settings {
                    speed: self.speed,
                    quality,
                    options: Vec::new(),
                };
                let bytes = avif::encode(source, &settings)?;
                let decoded = avif::decode(&bytes)?;
                self.gate(metric, source, bytes, &decoded)
            },
        )
    }

    fn jxl_lossy(
        &self,
        metric: &dyn Metric,
        source: &Img,
    ) -> Result<(Option<(f32, Lossy)>, usize), Error> {
        let start = self
            .distances
            .iter()
            .position(|&d| (d - self.distance_start).abs() < 1e-6)
            .unwrap_or(0);
        seek(
            &self.distances,
            start,
            self.margin_step,
            self.stop_margin,
            |d| {
                let bytes = jxl::encode(source, jxl::Mode::Lossy(d), self.jxl_effort)?;
                let decoded = jxl::decode(&bytes)?;
                self.gate(metric, source, bytes, &decoded)
            },
        )
    }

    /// Lossless JPEG XL of the pixels, if it decodes to them exactly.
    fn jxl_lossless(&self, source: &Img) -> Result<Option<Vec<u8>>, Error> {
        let out = jxl::encode(source, jxl::Mode::Lossless, self.jxl_effort)?;
        Ok(jxl::decode(&out)?.same_pixels(source).then_some(out))
    }

    /// Chooses how to store the picture whose file is `bytes`: the smallest of the candidates
    /// that pass, or the source when none is smaller. Scores with the CPU metric.
    #[must_use]
    pub fn choose(&self, bytes: &[u8]) -> Choice {
        self.choose_with(bytes, &Cpu)
    }

    /// [`Config::choose`], scoring with `metric`. A metric error counts as the codec's: on
    /// libaom's side it sends the picture to JPEG XL, so a metric that can fail should fall back
    /// on its own.
    #[must_use]
    pub fn choose_with(&self, bytes: &[u8], metric: &dyn Metric) -> Choice {
        let source = |note: String, size, probes| Choice {
            codec: Codec::Source,
            param: None,
            bytes: bytes.to_vec(),
            score: None,
            probes,
            note: Some(note),
            size,
            resized: None,
        };
        if img::is_animated_gif(bytes) {
            return source("animated GIF".to_owned(), None, 0);
        }
        let picture = match img::decode(bytes) {
            Ok(p) => p,
            Err(e) => return source(format!("decode: {e}"), None, 0),
        };
        let size = Some((picture.width, picture.height));
        let start = self.quality_start(
            bytes.len(),
            f64::from(picture.width) * f64::from(picture.height),
            Format::sniff(bytes) == Format::Jpeg,
        );
        let resized = picture.fit(self.short_edge, self.short_margin);
        let picture = match resized {
            Some((w, h)) => picture.resized(w, h),
            None => picture,
        };
        let mut notes = Vec::new();
        let mut probes = 0;
        let mut best: Option<Choice> = None;
        let mut offer = |c: Choice| {
            if best.as_ref().is_none_or(|b| c.bytes.len() < b.bytes.len()) {
                best = Some(c);
            }
        };
        let lossless = |codec, out: Vec<u8>| Choice {
            codec,
            param: None,
            bytes: out,
            score: None,
            probes: 0,
            note: None,
            size,
            resized,
        };
        let lossy = |codec, param: f64, l: Lossy| Choice {
            codec,
            param: Some(param),
            bytes: l.bytes,
            score: Some(l.score),
            probes: 0,
            note: None,
            size,
            resized,
        };
        // The lossless floor (a resized picture has no source to transcode).
        let jpeg = resized.is_none() && Format::sniff(bytes) == Format::Jpeg;
        let few = picture.colours(self.lossless_colours + 1) <= self.lossless_colours;
        if jpeg {
            probes += 1;
            match jxl::encode_jpeg(bytes, self.jxl_effort)
                .and_then(|t| Ok((jxl::reconstruct_jpeg(&t)? == bytes).then_some(t)))
            {
                Ok(Some(t)) => offer(lossless(Codec::JxlJpeg, t)),
                Ok(None) => notes.push("jxl-jpeg: not exact".to_owned()),
                Err(e) => notes.push(format!("jxl-jpeg: {e}")),
            }
        }
        if few {
            probes += 1;
            match self.jxl_lossless(&picture) {
                Ok(Some(t)) => offer(lossless(Codec::JxlLossless, t)),
                Ok(None) => notes.push("jxl-lossless: not exact".to_owned()),
                Err(e) => notes.push(format!("jxl-lossless: {e}")),
            }
        }
        // The lossy search; JPEG XL's when libaom refuses the picture.
        match self.avif(metric, &picture, start) {
            Ok((found, n)) => {
                probes += n;
                if let Some((q, l)) = found {
                    offer(lossy(Codec::Avif, f64::from(q), l));
                }
            }
            Err(e) => {
                notes.push(format!("avif: {e}"));
                match self.jxl_lossy(metric, &picture) {
                    Ok((found, n)) => {
                        probes += n;
                        if let Some((d, l)) = found {
                            offer(lossy(Codec::JxlLossy, f64::from(d), l));
                        }
                    }
                    Err(e) => notes.push(format!("jxl-lossy: {e}")),
                }
                if !few && !jpeg {
                    probes += 1;
                    match self.jxl_lossless(&picture) {
                        Ok(Some(t)) => offer(lossless(Codec::JxlLossless, t)),
                        Ok(None) => notes.push("jxl-lossless: not exact".to_owned()),
                        Err(e) => notes.push(format!("jxl-lossless: {e}")),
                    }
                }
            }
        }
        let note = (!notes.is_empty()).then(|| notes.join("; "));
        // A resized picture takes its smallest candidate even when the source is smaller.
        match best.filter(|b| resized.is_some() || b.bytes.len() < bytes.len()) {
            Some(b) => Choice { probes, note, ..b },
            None => source(
                note.unwrap_or_else(|| "nothing smaller".to_owned()),
                size,
                probes,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[expect(clippy::disallowed_macros, reason = "tests assert")]
    fn search_finds_the_last_pass() -> Result<(), Error> {
        let grid: Vec<u32> = (0..20).collect();
        for cut in 0..=20 {
            for start in [0, 9, 19] {
                let (best, probes) = search(&grid, start, |p| Ok((p < cut, p)))?;
                assert_eq!(best.map(|(p, _)| p), cut.checked_sub(1));
                assert!(probes <= 7, "cut {cut} start {start}: {probes} probes");
            }
        }
        Ok(())
    }

    #[test]
    #[expect(clippy::disallowed_macros, reason = "tests assert")]
    fn seek_finds_the_last_pass() -> Result<(), Error> {
        let grid: Vec<u32> = (0..24).collect();
        // Margins falling at different rates, straight and curved, crossing anywhere.
        let shapes: [&dyn Fn(f64) -> f64; 3] = [&|x| -0.5 * x, &|x| -1.7 * x, &|x| {
            -0.05 * x * x.abs() - 0.1 * x
        }];
        let mut total = 0;
        for shape in shapes {
            for cut in 0..=24_u32 {
                for start in [0, 11, 23] {
                    let margin = |p: u32| shape(f64::from(p) - f64::from(cut) + 0.5);
                    let (best, probes) = seek(&grid, start, -0.5, 0.0, |p| Ok((margin(p), p)))?;
                    assert_eq!(best.map(|(p, _)| p), cut.checked_sub(1), "cut {cut}");
                    assert!(probes <= 8, "cut {cut} start {start}: {probes} probes");
                    total += probes;
                }
            }
        }
        assert!(total < 3 * 25 * 3 * 4, "{total} probes in all");
        Ok(())
    }

    #[test]
    #[expect(clippy::disallowed_macros, reason = "tests assert")]
    fn a_thin_margin_ends_the_search() -> Result<(), Error> {
        let grid: Vec<u32> = (0..24).collect();
        for cut in 1..=24_u32 {
            for start in [0, 11, 23] {
                let margin = |p: u32| -0.5 * (f64::from(p) - f64::from(cut) + 0.5);
                let (exact, n) = seek(&grid, start, -0.5, 0.0, |p| Ok((margin(p), p)))?;
                let (quick, m) = seek(&grid, start, -0.5, 0.3, |p| Ok((margin(p), p)))?;
                // The crossing here leaves the last pass 0.25 over, under 0.3: found, and alone.
                assert_eq!(
                    quick.map(|q| q.0),
                    exact.map(|e| e.0),
                    "cut {cut} start {start}"
                );
                assert!(m <= n, "cut {cut} start {start}: {m} vs {n}");
            }
        }
        // A wide margin still confirms with the next setting down.
        let (best, n) = seek(&grid, 5, -0.5, 0.3, |p| {
            Ok((if p < 6 { 2.0 } else { -2.0 }, p))
        })?;
        assert_eq!(best.map(|b| b.0), Some(5));
        assert!(n > 1, "{n} probes");
        Ok(())
    }

    #[test]
    #[expect(clippy::disallowed_macros, reason = "tests assert")]
    fn the_start_follows_bits_per_pixel() {
        let c = Chooser::default();
        // 1 bit per pixel, 5, 9 and 40.
        let px = 1000.0 * 1000.0;
        assert_eq!(c.quality_start(125_000, px, true), 64);
        assert_eq!(c.quality_start(625_000, px, true), 72);
        assert_eq!(c.quality_start(1_125_000, px, true), 78);
        assert_eq!(c.quality_start(1_125_000, px, false), 72);
        assert_eq!(c.quality_start(5_000_000, px, false), 80);
        let empty = Chooser(Config {
            quality_starts_other: Vec::new(),
            ..Config::default()
        });
        assert_eq!(empty.quality_start(1, px, false), 78);
    }

    #[test]
    #[expect(clippy::disallowed_macros, reason = "tests assert")]
    fn the_gpu_gates_are_higher() {
        let (cpu, gpu) = (Gates::default(), Gates::gpu());
        assert!(gpu.mean > cpu.mean && gpu.worst > cpu.worst);
        assert_eq!(gpu.tile, cpu.tile);
        assert_eq!(Cpu.name(), "ssimulacra2 crate (cpu)");
    }

    #[test]
    #[expect(clippy::disallowed_macros, reason = "tests assert")]
    fn a_tiny_flat_picture_is_kept_or_lossless() {
        let png = {
            let mut out = Vec::new();
            let im = image::RgbImage::from_pixel(16, 16, image::Rgb([10, 200, 30]));
            let ok = image::DynamicImage::ImageRgb8(im)
                .write_to(&mut io::Cursor::new(&mut out), image::ImageFormat::Png)
                .is_ok();
            assert!(ok);
            out
        };
        let c = Chooser::default().choose(&png);
        assert!(c.bytes.len() <= png.len());
        assert!(matches!(
            c.codec,
            Codec::Source | Codec::JxlLossless | Codec::Avif
        ));
    }
}
