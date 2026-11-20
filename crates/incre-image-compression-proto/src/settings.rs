//! The encoding settings a job carries.

use serde::{Deserialize, Serialize};

/// The gates every lossy result must pass, and the metric's tile.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Gates {
    /// The least area-weighted mean SSIMULACRA2 over the tiles.
    pub mean: f64,
    /// The least SSIMULACRA2 of the worst tile.
    pub worst: f64,
    /// The tile's side in pixels.
    pub tile: usize,
}

/// 87/80 rather than 90/85: about 28% smaller, and in blind side-by-side and flip tests
/// (60 pictures, 1:1 crops at the worst tile) no more visible than 90/85; 85/78 was.
impl Default for Gates {
    fn default() -> Self {
        Self {
            mean: 87.0,
            worst: 80.0,
            tile: 256,
        }
    }
}

impl Gates {
    /// The default gates moved up by 0.3 for IncreDex's GPU scorer (f64-exact SSIMULACRA2): its scores
    /// match an f64 computation, where the crate's (which the blind tests used) drift about 0.3
    /// lower near the gates, so these cut where the tests did.
    #[must_use]
    pub fn gpu() -> Self {
        let d = Self::default();
        Self {
            mean: d.mean + 0.3,
            worst: d.worst + 0.3,
            ..d
        }
    }
}

/// How a picture is encoded: sent with each job, every field optional (the defaults are the
/// ones the benchmark settled on).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The gates.
    pub gates: Gates,
    /// libaom's speed.
    pub speed: i32,
    /// The AVIF qualities to search, best first.
    pub qualities: Vec<i32>,
    /// Where the AVIF search starts, by the source file's bits per pixel (at its decoded size):
    /// `(at most, quality)`, ascending, the last for anything above. Fitted on the repack's
    /// first 3760 pictures at the GPU gates (each band's median quality).
    pub quality_starts_jpeg: Vec<(f64, i32)>,
    /// [`Config::quality_starts_jpeg`] for sources that aren't JPEG.
    pub quality_starts_other: Vec<(f64, i32)>,
    /// A pass with less margin than this ends a search without trying the next setting down
    /// (0 always tries it). On 60 pictures 0.5 took 13% fewer encodes and changed 2 qualities
    /// by a step, for 0.03% more bytes.
    pub stop_margin: f64,
    /// How much the gates' margin typically falls per step down a grid (negative): where the
    /// search guesses the crossing from one probe.
    pub margin_step: f64,
    /// The Butteraugli distances JPEG XL's lossy fallback searches, best first.
    pub distances: Vec<f32>,
    /// The distance that search starts at.
    pub distance_start: f32,
    /// libjxl's effort.
    pub jxl_effort: i64,
    /// Lossless JPEG XL is tried on pictures with at most this many colours (alpha included).
    pub lossless_colours: usize,
    /// Pictures whose short edge is over `short_edge × short_margin` are resized to this short
    /// edge first (aspect kept); then the source is never kept, nor its JPEG transcoded.
    pub short_edge: u32,
    /// See `short_edge`.
    pub short_margin: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            gates: Gates::default(),
            speed: 6,
            qualities: (25..=48).rev().map(|q| q * 2).collect(),
            quality_starts_jpeg: vec![
                (1.08, 64),
                (4.36, 68),
                (6.06, 72),
                (7.62, 74),
                (f64::MAX, 78),
            ],
            quality_starts_other: vec![
                (3.67, 60),
                (4.95, 62),
                (5.92, 64),
                (6.9, 66),
                (8.55, 68),
                (10.85, 72),
                (12.83, 76),
                (f64::MAX, 80),
            ],
            stop_margin: 0.5,
            margin_step: -0.5,
            distances: vec![
                0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0, 1.2, 1.4, 1.6, 1.8, 2.0, 2.3,
                2.6, 3.0, 3.5, 4.0,
            ],
            distance_start: 1.0,
            jxl_effort: 7,
            lossless_colours: 256,
            short_edge: 2048,
            short_margin: 4.0 / 3.0,
        }
    }
}
