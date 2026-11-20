//! The frames `incre-image-compression-server work` and `incre-image-compression-server serve` pass over their local unix socket.
//!
//! Each worker thread holds a connection of its own and asks for one job at a time: it sends
//! [`ToServe::Next`], the server answers [`ToWork::Job`] when there is one (the connection
//! simply waits until then), and the thread answers [`ToServe::Done`] or [`ToServe::Failed`]
//! before asking again. A connection that drops mid-job puts the job back in the queue.
//!
//! A frame is a JSON header and a body of bytes: the header's length (`u32`, big-endian), the
//! header, the body's length (`u64`, big-endian), the body.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{Config, Record};

/// The longest header taken.
pub const MAX_HEADER: u32 = 1 << 20;

/// What a worker thread says.
#[expect(clippy::large_enum_variant, reason = "one frame per picture")]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ToServe {
    /// Ready for a job; the first frame of a connection also names the metric.
    Next {
        /// The metric the worker scores with.
        metric: String,
    },
    /// A job is done; the body is the result.
    Done {
        /// The job's key.
        key: String,
        /// What it became.
        record: Record,
    },
    /// A job failed; no body.
    Failed {
        /// The job's key.
        key: String,
        /// Why.
        error: String,
    },
}

/// What the server says to a worker thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ToWork {
    /// A job; the body is the picture.
    Job {
        /// The job's key.
        key: String,
        /// How to encode it.
        settings: Config,
    },
}

/// Writes a frame of `header` and `body`.
///
/// # Errors
///
/// If writing fails, or the header doesn't serialise or is over [`MAX_HEADER`].
pub fn write<W: Write, H: Serialize>(w: &mut W, header: &H, body: &[u8]) -> io::Result<()> {
    let head = serde_json::to_vec(header).map_err(io::Error::other)?;
    let len = u32::try_from(head.len())
        .ok()
        .filter(|&n| n <= MAX_HEADER)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame header too long"))?;
    let body_len = u64::try_from(body.len()).map_err(io::Error::other)?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&head)?;
    w.write_all(&body_len.to_be_bytes())?;
    w.write_all(body)?;
    w.flush()
}

/// Reads a frame: its header, and its body if that is at most `max_body` bytes.
///
/// # Errors
///
/// If reading fails, the connection closes, the header doesn't parse, or a length is over its
/// limit.
pub fn read<R: Read, H: DeserializeOwned>(r: &mut R, max_body: u64) -> io::Result<(H, Vec<u8>)> {
    let mut n4 = [0; 4];
    r.read_exact(&mut n4)?;
    let len = u32::from_be_bytes(n4);
    if len > MAX_HEADER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame header too long",
        ));
    }
    let mut head = vec![0; usize::try_from(len).map_err(io::Error::other)?];
    r.read_exact(&mut head)?;
    let header =
        serde_json::from_slice(&head).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut n8 = [0; 8];
    r.read_exact(&mut n8)?;
    let body_len = u64::from_be_bytes(n8);
    if body_len > max_body {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame body too long",
        ));
    }
    let mut body = vec![0; usize::try_from(body_len).map_err(io::Error::other)?];
    r.read_exact(&mut body)?;
    Ok((header, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[expect(clippy::disallowed_macros, reason = "tests assert")]
    fn a_frame_reads_back() -> io::Result<()> {
        let mut buf = Vec::new();
        let job = ToWork::Job {
            key: "a.1".into(),
            settings: Config::default(),
        };
        write(&mut buf, &job, b"pixels")?;
        let (back, body): (ToWork, _) = read(&mut io::Cursor::new(&buf), 100)?;
        assert_eq!(back, job);
        assert_eq!(body, b"pixels");
        // a body over the limit is refused
        let e = read::<_, ToWork>(&mut io::Cursor::new(&buf), 3);
        assert!(e.is_err());
        Ok(())
    }

    #[test]
    #[expect(clippy::disallowed_macros, reason = "tests assert")]
    fn keys_are_checked() {
        assert!(crate::valid_key("0597-369597.jpg"));
        assert!(!crate::valid_key(""));
        assert!(!crate::valid_key("a/b"));
        assert!(!crate::valid_key(&"x".repeat(129)));
    }

    #[test]
    #[expect(clippy::disallowed_macros, reason = "tests assert")]
    fn settings_default_what_is_missing() -> Result<(), serde_json::Error> {
        let c: Config = serde_json::from_str(r#"{"speed": 8, "gates": {"mean": 90.0}}"#)?;
        assert_eq!(c.speed, 8);
        assert_eq!(c.gates.mean, 90.0);
        assert_eq!(c.gates.worst, Config::default().gates.worst);
        assert!(serde_json::from_str::<Config>(r#"{"sped": 8}"#).is_err());
        // the defaults survive JSON, f64::MAX bands included
        let d = Config::default();
        let back: Config = serde_json::from_str(&serde_json::to_string(&d)?)?;
        assert_eq!(back, d);
        Ok(())
    }
}
