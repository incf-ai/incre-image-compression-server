//! SSIMULACRA2 over tiles, so a small damaged area (a face's line art, text) isn't averaged
//! away by a large flat background.

use ssimulacra2::{LinearRgb, compute_frame_ssimulacra2};

use crate::Error;
use crate::img::Img;

/// A tile's place and score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tile {
    /// Left edge.
    pub x: usize,
    /// Top edge.
    pub y: usize,
    /// Width.
    pub w: usize,
    /// Height.
    pub h: usize,
    /// Its SSIMULACRA2 score.
    pub score: f64,
}

/// The scores of one comparison.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Score {
    /// Area-weighted mean of the tiles.
    pub mean: f64,
    /// The worst tile.
    pub worst: Tile,
}

/// Cuts `len` into spans of about `tile`; a remainder under half a tile joins the last span.
pub(crate) fn spans(len: usize, tile: usize) -> Vec<(usize, usize)> {
    let tile = tile.max(8);
    let mut out = Vec::new();
    let mut at = 0;
    while at < len {
        let rest = len - at;
        let w = if rest < tile + tile / 2 { rest } else { tile };
        out.push((at, w));
        at += w;
    }
    out
}

/// Scores `distorted` against `source` per tile, on linear light composited on `background`.
/// Pictures under 8 pixels a side are padded by edge repetition.
///
/// # Errors
///
/// If the sizes differ.
pub fn score(source: &Img, distorted: &Img, tile: usize, background: f32) -> Result<Score, Error> {
    if (source.width, source.height) != (distorted.width, distorted.height) {
        return Err(Error::new("metric: sizes differ"));
    }
    let width = usize::try_from(source.width).map_err(|_| Error::new("metric: width"))?;
    let height = usize::try_from(source.height).map_err(|_| Error::new("metric: height"))?;
    let mut total = 0.0;
    let mut area = 0.0;
    let mut worst: Option<Tile> = None;
    for &(y, h) in &spans(height, tile) {
        for &(x, w) in &spans(width, tile) {
            let ta = source.linear_rect(x, y, w, h, background);
            let tb = distorted.linear_rect(x, y, w, h, background);
            let (ta, tb, pw, ph) = pad(ta, tb, w, h);
            let ta =
                LinearRgb::new(ta, pw, ph).map_err(|e| Error::new(format!("metric: {e:?}")))?;
            let tb =
                LinearRgb::new(tb, pw, ph).map_err(|e| Error::new(format!("metric: {e:?}")))?;
            let s = compute_frame_ssimulacra2(ta, tb)
                .map_err(|e| Error::new(format!("metric: {e}")))?;
            let px = f64::from(u32::try_from(w * h).unwrap_or(u32::MAX));
            total += s * px;
            area += px;
            if worst.is_none_or(|t| s < t.score) {
                worst = Some(Tile {
                    x,
                    y,
                    w,
                    h,
                    score: s,
                });
            }
        }
    }
    let worst = worst.ok_or_else(|| Error::new("metric: empty picture"))?;
    Ok(Score {
        mean: total / area,
        worst,
    })
}

/// Repeats the last row and column until both sides are at least 8.
fn pad(
    a: Vec<[f32; 3]>,
    b: Vec<[f32; 3]>,
    w: usize,
    h: usize,
) -> (Vec<[f32; 3]>, Vec<[f32; 3]>, usize, usize) {
    if w >= 8 && h >= 8 {
        return (a, b, w, h);
    }
    let (nw, nh) = (w.max(8), h.max(8));
    let grow = |v: &[[f32; 3]]| -> Vec<[f32; 3]> {
        (0..nh)
            .flat_map(|y| (0..nw).map(move |x| (x.min(w - 1), y.min(h - 1))))
            .map(|(x, y)| v.get(y * w + x).copied().unwrap_or([0.0; 3]))
            .collect()
    };
    (grow(&a), grow(&b), nw, nh)
}
