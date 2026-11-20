//! What `incre-image-compression-server` and its clients say to each other: the HTTP API's paths, headers and JSON,
//! the encoding settings a job carries, and the frames `incre-image-compression-server serve` and `incre-image-compression-server work`
//! pass over their local socket.
//!
//! # The HTTP API
//!
//! Every call but `GET /healthz` needs `Authorization: Bearer <token>`. Nothing is written to
//! disk: jobs and results live in the server's RAM, and a restart forgets them (a fetch then
//! answers 404 and the client submits again). Every call is short; a client waits for a result
//! by fetching with `?wait=` again, so a server starved of CPU never holds a socket for long.
//!
//! - `PUT /v1/jobs/{key}`: the body is the picture; [`SETTINGS_HEADER`] optionally carries a
//!   [`Config`] as JSON (missing fields take their defaults). 202 and a [`JobInfo`] when queued;
//!   200 and the job's [`JobInfo`] if `key` is already known (the body is ignored, so a retried
//!   upload is harmless); 503 with `Retry-After` when the queue is full; 413 when the picture is
//!   over [`Status::max_job_bytes`].
//! - `GET /v1/jobs/{key}?wait=S`: waits up to `S` seconds (at most [`MAX_WAIT_S`]) for the job.
//!   200 with the encoded bytes as the body and a [`Record`] in [`RECORD_HEADER`] when done; 202
//!   and a [`JobInfo`] while queued or running; 422 and an [`ErrorBody`] if it failed; 404 if
//!   the server doesn't know `key`.
//! - `DELETE /v1/jobs/{key}`: forgets the job and its result (204). Fetch, then delete; results
//!   nobody deletes go after [`Status::result_ttl_s`].
//! - `GET /v1/status`: a [`Status`].

pub mod frame;
pub mod settings;

use serde::{Deserialize, Serialize};

pub use settings::{Config, Gates};

/// The API's version: in its paths and in [`Status::protocol`].
pub const PROTOCOL: u32 = 1;

/// The header carrying a job's [`Config`] as JSON.
pub const SETTINGS_HEADER: &str = "x-incre-compress-settings";

/// The header carrying a finished job's [`Record`] as JSON.
pub const RECORD_HEADER: &str = "x-incre-compress-record";

/// The longest a fetch may wait, in seconds.
pub const MAX_WAIT_S: u64 = 120;

/// The longest a job key may be.
pub const MAX_KEY_LEN: usize = 128;

/// Whether `key` is a usable job key: 1 to [`MAX_KEY_LEN`] of `A-Z a-z 0-9 . _ -`.
#[must_use]
pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY_LEN
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Where a job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Waiting for a worker.
    Queued,
    /// Being encoded.
    Running,
    /// Encoded; the result waits to be fetched.
    Done,
    /// The encoder failed.
    Failed,
}

/// A job's state, as `PUT` and a pending `GET` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobInfo {
    /// The job's key.
    pub key: String,
    /// Where it is.
    pub state: State,
    /// Jobs ahead of it in the queue (0 when not queued).
    pub position: usize,
}

/// What a finished picture became.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The codec: `source` (the bytes sent, unchanged), `jxl-jpeg` (a JPEG's lossless JPEG XL
    /// transcode), `jxl-lossless`, `jxl-lossy` or `avif`.
    pub codec: String,
    /// The stored file's extension (`avif`, `jxl`), or `None` when the source is kept.
    pub ext: Option<String>,
    /// The quality (AVIF) or Butteraugli distance (lossy JPEG XL).
    pub param: Option<f64>,
    /// The area-weighted mean SSIMULACRA2 over the tiles, for the lossy codecs.
    pub mean: Option<f64>,
    /// The worst tile's SSIMULACRA2, for the lossy codecs.
    pub min: Option<f64>,
    /// The decoded size, if the source decoded.
    pub width: Option<u32>,
    /// See `width`.
    pub height: Option<u32>,
    /// The stored size, when the picture was resized.
    pub resized: Option<(u32, u32)>,
    /// Encodes tried.
    pub probes: usize,
    /// Why the source was kept or a fallback taken, if so.
    pub note: Option<String>,
    /// The metric that scored it.
    pub metric: String,
    /// The source's size in bytes.
    pub source_size: u64,
    /// The result's size in bytes.
    pub size: u64,
    /// The result's SHA-256, in hex.
    pub sha256: String,
    /// CPU seconds the encode took.
    pub cpu_s: f64,
    /// Wall seconds the encode took (long when the machine was busy).
    pub wall_s: f64,
}

/// An error's words, as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// What went wrong.
    pub error: String,
}

/// The server's state, from `GET /v1/status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// The server's version.
    pub version: String,
    /// [`PROTOCOL`].
    pub protocol: u32,
    /// The metric its workers score with.
    pub metric: String,
    /// Worker threads connected.
    pub workers: usize,
    /// Jobs waiting for a worker.
    pub queued: usize,
    /// Jobs being encoded.
    pub running: usize,
    /// Finished jobs whose results wait to be fetched (failed ones included).
    pub done: usize,
    /// Bytes held in RAM: sources not yet encoded, and results not yet deleted.
    pub held_bytes: u64,
    /// The most bytes it holds before refusing jobs.
    pub max_held_bytes: u64,
    /// The largest picture it takes.
    pub max_job_bytes: u64,
    /// The most jobs it holds, in any state.
    pub max_jobs: usize,
    /// How long a result nobody deletes is kept, in seconds.
    pub result_ttl_s: u64,
    /// Jobs finished since the start.
    pub finished_total: u64,
    /// Jobs failed since the start.
    pub failed_total: u64,
    /// CPU seconds encoding took since the start.
    pub cpu_s_total: f64,
    /// Seconds since the start.
    pub uptime_s: u64,
}
