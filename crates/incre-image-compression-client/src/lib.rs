//! A blocking client for `incre-image-compression-server`, patient with a server that only runs on spare CPU time.
//!
//! [`Client::encode`] does the whole round: it submits the picture under a key made from its
//! bytes and settings, waits for the result in short long-polls, resubmits if the server forgot
//! the job (a restart), backs off while the queue is full or the server can't be reached, then
//! fetches and deletes the result. The lower-level calls are there for a caller that wants to
//! keep many jobs in flight.

use std::fmt;
use std::time::{Duration, Instant};

pub use incre_image_compression_proto::{Config, Gates, JobInfo, Record, State, Status};
use incre_image_compression_proto::{ErrorBody, MAX_WAIT_S, RECORD_HEADER, SETTINGS_HEADER};
use sha2::{Digest, Sha256};
use ureq::Agent;
use ureq::http::StatusCode;

/// What went wrong.
#[derive(Debug)]
pub enum Error {
    /// The server couldn't be reached, or the connection broke.
    Transport(ureq::Error),
    /// The server refused the token.
    Unauthorized,
    /// The picture is over the server's size limit.
    TooLarge,
    /// The encoder failed on the picture; retrying won't help.
    Failed(String),
    /// The server answered something unexpected.
    Protocol(String),
    /// [`Client::patience`] ran out.
    GaveUp(Box<Error>),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "transport: {e}"),
            Self::Unauthorized => f.write_str("the server refused the token"),
            Self::TooLarge => f.write_str("the picture is over the server's size limit"),
            Self::Failed(e) => write!(f, "the encoder failed: {e}"),
            Self::Protocol(e) => write!(f, "protocol: {e}"),
            Self::GaveUp(e) => write!(f, "gave up waiting; last: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<ureq::Error> for Error {
    fn from(e: ureq::Error) -> Self {
        Self::Transport(e)
    }
}

/// What a submission did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submitted {
    /// Queued now, or known already.
    Queued(JobInfo),
    /// The server is full; try again after this long.
    Full(Duration),
}

/// What a fetch found.
#[derive(Debug, Clone, PartialEq)]
pub enum Fetched {
    /// The result and its record.
    Done(Box<Record>, Vec<u8>),
    /// Not done yet.
    Pending(JobInfo),
    /// The encoder failed.
    Failed(String),
    /// The server doesn't know the key (never submitted, deleted, expired, or restarted).
    Unknown,
}

/// A job key from a picture's bytes and its settings: the same pair always gets the same key,
/// so a resubmission finds the job already there.
#[must_use]
pub fn job_key(bytes: &[u8], settings: &Config) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.update(serde_json::to_vec(settings).unwrap_or_default());
    h.finalize()
        .iter()
        .take(20)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A connection to one server.
#[derive(Debug, Clone)]
pub struct Client {
    agent: Agent,
    base: String,
    auth: String,
    /// How long [`Client::encode`] keeps trying through outages and full queues before giving
    /// up. Time a job spends queued or encoding doesn't count against it.
    pub patience: Duration,
    /// The longest a single long-poll waits.
    pub poll: Duration,
    /// The largest result accepted.
    pub max_result: u64,
}

impl Client {
    /// A client for the server at `base` (like `http://buildbox:8790`) with `token`.
    #[must_use]
    pub fn new(base: &str, token: &str) -> Self {
        let agent = Agent::config_builder()
            .http_status_as_error(false)
            // a box short of CPU may take a while to accept, read or answer
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_send_body(Some(Duration::from_secs(600)))
            .timeout_recv_response(Some(Duration::from_secs(120)))
            .timeout_recv_body(Some(Duration::from_secs(600)))
            .max_idle_age(Duration::from_secs(50))
            .build()
            .into();
        Self {
            agent,
            base: base.trim_end_matches('/').to_owned(),
            auth: format!("Bearer {}", token.trim()),
            patience: Duration::from_secs(6 * 3600),
            poll: Duration::from_secs(60),
            max_result: 1 << 30,
        }
    }

    fn url(&self, key: &str) -> String {
        format!("{}/v1/jobs/{key}", self.base)
    }

    /// Submits `bytes` under `key`.
    ///
    /// # Errors
    ///
    /// On a transport error, a refused token, a picture over the limit, or an odd answer.
    pub fn submit(&self, key: &str, bytes: &[u8], settings: &Config) -> Result<Submitted, Error> {
        let settings =
            serde_json::to_string(settings).map_err(|e| Error::Protocol(e.to_string()))?;
        let mut r = self
            .agent
            .put(&self.url(key))
            .header("authorization", &self.auth)
            .header(SETTINGS_HEADER, &settings)
            .header("content-type", "application/octet-stream")
            .send(bytes)?;
        match r.status() {
            StatusCode::OK | StatusCode::ACCEPTED => Ok(Submitted::Queued(json(&mut r)?)),
            StatusCode::SERVICE_UNAVAILABLE => {
                let after = r
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(30);
                Ok(Submitted::Full(Duration::from_secs(after)))
            }
            StatusCode::PAYLOAD_TOO_LARGE => Err(Error::TooLarge),
            _ => Err(odd(&mut r)),
        }
    }

    /// Fetches `key`, waiting up to `wait` (at most two minutes) for it to finish.
    ///
    /// # Errors
    ///
    /// On a transport error, a refused token, or an odd answer.
    pub fn fetch(&self, key: &str, wait: Duration) -> Result<Fetched, Error> {
        let wait = wait.as_secs().min(MAX_WAIT_S);
        let mut r = self
            .agent
            .get(&format!("{}?wait={wait}", self.url(key)))
            .header("authorization", &self.auth)
            .config()
            .timeout_recv_response(Some(Duration::from_secs(wait + 120)))
            .build()
            .call()?;
        match r.status() {
            StatusCode::OK => {
                let record = r
                    .headers()
                    .get(RECORD_HEADER)
                    .ok_or_else(|| Error::Protocol(format!("no {RECORD_HEADER}")))
                    .and_then(|h| {
                        serde_json::from_slice(h.as_bytes())
                            .map_err(|e| Error::Protocol(format!("{RECORD_HEADER}: {e}")))
                    })?;
                let body = r
                    .body_mut()
                    .with_config()
                    .limit(self.max_result)
                    .read_to_vec()?;
                Ok(Fetched::Done(Box::new(record), body))
            }
            StatusCode::ACCEPTED => Ok(Fetched::Pending(json(&mut r)?)),
            StatusCode::UNPROCESSABLE_ENTITY => Ok(Fetched::Failed(
                json::<ErrorBody>(&mut r).map_or_else(|e| e.to_string(), |b| b.error),
            )),
            StatusCode::NOT_FOUND => Ok(Fetched::Unknown),
            _ => Err(odd(&mut r)),
        }
    }

    /// Forgets `key` and its result; `false` if the server didn't know it.
    ///
    /// # Errors
    ///
    /// On a transport error, a refused token, or an odd answer.
    pub fn delete(&self, key: &str) -> Result<bool, Error> {
        let mut r = self
            .agent
            .delete(&self.url(key))
            .header("authorization", &self.auth)
            .call()?;
        match r.status() {
            StatusCode::NO_CONTENT => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            _ => Err(odd(&mut r)),
        }
    }

    /// The server's state.
    ///
    /// # Errors
    ///
    /// On a transport error, a refused token, or an odd answer.
    pub fn status(&self) -> Result<Status, Error> {
        let mut r = self
            .agent
            .get(&format!("{}/v1/status", self.base))
            .header("authorization", &self.auth)
            .call()?;
        match r.status() {
            StatusCode::OK => json(&mut r),
            _ => Err(odd(&mut r)),
        }
    }

    /// Encodes one picture, through restarts, outages and full queues: see the crate's docs.
    ///
    /// # Errors
    ///
    /// [`Error::Failed`] if the encoder failed on it, [`Error::Unauthorized`] or
    /// [`Error::TooLarge`] at once, and [`Error::GaveUp`] when the server stayed unreachable or
    /// full for longer than [`Client::patience`].
    pub fn encode(&self, bytes: &[u8], settings: &Config) -> Result<(Record, Vec<u8>), Error> {
        let key = job_key(bytes, settings);
        let mut trouble: Option<Instant> = None;
        let mut backoff = Duration::from_secs(1);
        let mut submitted = false;
        loop {
            let step = if submitted {
                self.fetch(&key, self.poll)
            } else {
                self.submit(&key, bytes, settings).map(|s| match s {
                    Submitted::Queued(info) => {
                        submitted = true;
                        Fetched::Pending(info)
                    }
                    Submitted::Full(after) => {
                        backoff = after;
                        Fetched::Unknown
                    }
                })
            };
            let wait = match step {
                Ok(Fetched::Done(record, body)) => {
                    // a failed delete only leaves the result to the server's TTL
                    let _left_to_the_ttl = self.delete(&key);
                    return Ok((*record, body));
                }
                Ok(Fetched::Failed(e)) => {
                    let _left_to_the_ttl = self.delete(&key);
                    return Err(Error::Failed(e));
                }
                Ok(Fetched::Pending(_)) => {
                    trouble = None;
                    backoff = Duration::from_secs(1);
                    continue;
                }
                Ok(Fetched::Unknown) if submitted => {
                    // the server restarted or dropped it: send it again
                    submitted = false;
                    continue;
                }
                Ok(Fetched::Unknown) => Error::Protocol("the queue is full".to_owned()),
                Err(e @ (Error::Unauthorized | Error::TooLarge | Error::Failed(_))) => {
                    return Err(e);
                }
                Err(e) => e,
            };
            let since = *trouble.get_or_insert_with(Instant::now);
            if since.elapsed() > self.patience {
                return Err(Error::GaveUp(Box::new(wait)));
            }
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    }
}

fn json<T: serde::de::DeserializeOwned>(
    r: &mut ureq::http::Response<ureq::Body>,
) -> Result<T, Error> {
    let b = r.body_mut().with_config().limit(1 << 20).read_to_vec()?;
    serde_json::from_slice(&b).map_err(|e| Error::Protocol(e.to_string()))
}

/// An unexpected answer as an error.
fn odd(r: &mut ureq::http::Response<ureq::Body>) -> Error {
    let status = r.status();
    if status == StatusCode::UNAUTHORIZED {
        return Error::Unauthorized;
    }
    let words = json::<ErrorBody>(r).map_or_else(|_| String::new(), |b| b.error);
    Error::Protocol(format!("{status}: {words}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[expect(clippy::disallowed_macros, reason = "tests assert")]
    fn keys_follow_bytes_and_settings() {
        let c = Config::default();
        let k = job_key(b"abc", &c);
        assert_eq!(k.len(), 40);
        assert!(incre_image_compression_proto::valid_key(&k));
        assert_eq!(k, job_key(b"abc", &c));
        assert_ne!(k, job_key(b"abd", &c));
        let fast = Config {
            speed: 8,
            ..Config::default()
        };
        assert_ne!(k, job_key(b"abc", &fast));
    }
}
