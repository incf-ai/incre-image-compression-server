//! `incre-image-compression-server serve`: the HTTP API, the token, and the jobs, all in RAM.
//!
//! Nothing touches the disk. Pictures wait in a queue; `incre-image-compression-server work` connects over the unix
//! socket, one connection per worker thread, and takes them one at a time. A result waits for
//! its client to fetch and delete it, or for the TTL. The client retries: a restart loses
//! everything, and a fetch of a key the server doesn't know answers 404.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State as Shared};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use incre_image_compression_proto::frame::{MAX_HEADER, ToServe, ToWork};
use incre_image_compression_proto::{
    Config, ErrorBody, JobInfo, MAX_WAIT_S, PROTOCOL, RECORD_HEADER, Record, SETTINGS_HEADER,
    State, Status, valid_key,
};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

use crate::config::Serve;
use crate::log;

/// How long a client told 503 should wait before trying again.
const RETRY_AFTER_S: u64 = 30;

/// A job and what became of it.
#[derive(Debug)]
struct Job {
    settings: Config,
    /// The picture, until it is encoded.
    source: Option<Bytes>,
    state: State,
    /// The worker connection encoding it.
    owner: Option<u64>,
    result: Option<(Record, Bytes)>,
    error: Option<String>,
    finished: Option<Instant>,
}

impl Job {
    fn held(&self) -> u64 {
        let n = self.source.as_ref().map_or(0, Bytes::len)
            + self.result.as_ref().map_or(0, |(_, b)| b.len());
        u64::try_from(n).unwrap_or(u64::MAX)
    }
}

#[derive(Debug, Default)]
struct Jobs {
    jobs: HashMap<String, Job>,
    queue: VecDeque<String>,
    held: u64,
    workers: usize,
    metric: String,
    finished_total: u64,
    failed_total: u64,
    cpu_s_total: f64,
}

impl Jobs {
    fn info(&self, key: &str, job: &Job) -> JobInfo {
        let position = if job.state == State::Queued {
            self.queue.iter().position(|k| k == key).unwrap_or(0)
        } else {
            0
        };
        JobInfo {
            key: key.to_owned(),
            state: job.state,
            position,
        }
    }

    fn remove(&mut self, key: &str) -> Option<Job> {
        let job = self.jobs.remove(key)?;
        self.held = self.held.saturating_sub(job.held());
        if job.state == State::Queued {
            self.queue.retain(|k| k != key);
        }
        Some(job)
    }

    /// Puts the jobs a dropped worker connection held back at the queue's front.
    fn requeue(&mut self, owner: u64) {
        for (key, job) in &mut self.jobs {
            if job.owner == Some(owner) && job.state == State::Running {
                job.state = State::Queued;
                job.owner = None;
                self.queue.push_front(key.clone());
            }
        }
    }
}

#[derive(Debug)]
struct App {
    cfg: Serve,
    jobs: Mutex<Jobs>,
    /// Bumped on every change, so waiters (fetches, idle workers) look again.
    changed: watch::Sender<u64>,
    started: Instant,
}

impl App {
    fn lock(&self) -> MutexGuard<'_, Jobs> {
        // a panic while holding the lock leaves plain data behind; carry on with it
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn bump(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }
}

pub fn run(cfg: Serve) -> io::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(serve(cfg))
}

async fn serve(cfg: Serve) -> io::Result<()> {
    let (changed, _) = watch::channel(0);
    let app = Arc::new(App {
        cfg,
        jobs: Mutex::new(Jobs::default()),
        changed,
        started: Instant::now(),
    });
    let workers = bind_socket(&app.cfg.socket)?;
    let http = tokio::net::TcpListener::bind(app.cfg.listen).await?;
    log!(
        "incre-image-compression-server serve {}: http on {}, workers on {}, {} MB held at most",
        env!("CARGO_PKG_VERSION"),
        app.cfg.listen,
        app.cfg.socket.display(),
        app.cfg.max_held >> 20
    );
    let _accept = tokio::spawn(accept_workers(workers, Arc::clone(&app)));
    let _reap = tokio::spawn(reap(Arc::clone(&app)));
    let router = router(Arc::clone(&app));
    tokio::select! {
        r = axum::serve(http, router) => r,
        r = stop_signal() => r,
    }
}

async fn stop_signal() -> io::Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        r = tokio::signal::ctrl_c() => r?,
        _ = term.recv() => {}
    }
    log!("incre-image-compression-server serve: stopping; jobs in RAM are dropped");
    Ok(())
}

/// Binds the worker socket, owner-only, replacing a stale one.
fn bind_socket(path: &std::path::Path) -> io::Result<UnixListener> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let l = UnixListener::bind(path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

fn router(app: Arc<App>) -> Router {
    let limit = usize::try_from(app.cfg.max_job).unwrap_or(usize::MAX);
    let api = Router::new()
        .route("/v1/jobs/{key}", get(fetch).put(submit).delete(forget))
        .route("/v1/status", get(status))
        .layer(middleware::from_fn_with_state(Arc::clone(&app), auth))
        .layer(DefaultBodyLimit::max(limit));
    Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .merge(api)
        .with_state(app)
}

/// Compares in time independent of where the strings differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn auth(Shared(app): Shared<Arc<App>>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| same(t.trim().as_bytes(), app.cfg.token.as_bytes()));
    if ok {
        next.run(req).await
    } else {
        error(StatusCode::UNAUTHORIZED, "a bad or missing token")
    }
}

fn error(code: StatusCode, words: &str) -> Response {
    (
        code,
        axum::Json(ErrorBody {
            error: words.to_owned(),
        }),
    )
        .into_response()
}

async fn submit(
    Shared(app): Shared<Arc<App>>,
    Path(key): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !valid_key(&key) {
        return error(
            StatusCode::BAD_REQUEST,
            "a key is 1 to 128 of A-Z a-z 0-9 . _ -",
        );
    }
    let settings = match headers.get(SETTINGS_HEADER) {
        None => Config::default(),
        Some(v) => match serde_json::from_slice::<Config>(v.as_bytes()) {
            Ok(c) => c,
            Err(e) => return error(StatusCode::BAD_REQUEST, &format!("{SETTINGS_HEADER}: {e}")),
        },
    };
    if body.is_empty() {
        return error(StatusCode::BAD_REQUEST, "an empty picture");
    }
    let size = u64::try_from(body.len()).unwrap_or(u64::MAX);
    let mut jobs = app.lock();
    if let Some(job) = jobs.jobs.get(&key) {
        let info = jobs.info(&key, job);
        return (StatusCode::OK, axum::Json(info)).into_response();
    }
    if jobs.jobs.len() >= app.cfg.max_jobs || jobs.held.saturating_add(size) > app.cfg.max_held {
        drop(jobs);
        let mut r = error(
            StatusCode::SERVICE_UNAVAILABLE,
            "the queue is full; try later",
        );
        let _ = r
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(RETRY_AFTER_S));
        return r;
    }
    jobs.held += size;
    jobs.queue.push_back(key.clone());
    let position = jobs.queue.len().saturating_sub(1);
    let _ = jobs.jobs.insert(
        key.clone(),
        Job {
            settings,
            source: Some(body),
            state: State::Queued,
            owner: None,
            result: None,
            error: None,
            finished: None,
        },
    );
    drop(jobs);
    app.bump();
    let info = JobInfo {
        key,
        state: State::Queued,
        position,
    };
    (StatusCode::ACCEPTED, axum::Json(info)).into_response()
}

#[derive(Debug, Deserialize)]
struct Wait {
    wait: Option<u64>,
}

async fn fetch(
    Shared(app): Shared<Arc<App>>,
    Path(key): Path<String>,
    Query(q): Query<Wait>,
) -> Response {
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(q.wait.unwrap_or(0).min(MAX_WAIT_S));
    let mut changed = app.changed.subscribe();
    loop {
        // look while subscribed, so a change after the look still wakes us
        let _ = changed.borrow_and_update();
        if let Some(r) = look(&app, &key) {
            return r;
        }
        match tokio::time::timeout_at(deadline, changed.changed()).await {
            Ok(Ok(())) => {}
            // out of time, or shutting down: say where it is
            Ok(Err(_)) | Err(_) => {
                return look(&app, &key).unwrap_or_else(|| {
                    let jobs = app.lock();
                    match jobs.jobs.get(&key) {
                        Some(job) => {
                            (StatusCode::ACCEPTED, axum::Json(jobs.info(&key, job))).into_response()
                        }
                        None => error(StatusCode::NOT_FOUND, "no such job"),
                    }
                });
            }
        }
    }
}

/// The answer for a job that is settled (done, failed or unknown), or `None` while it waits.
fn look(app: &App, key: &str) -> Option<Response> {
    let jobs = app.lock();
    let Some(job) = jobs.jobs.get(key) else {
        return Some(error(StatusCode::NOT_FOUND, "no such job"));
    };
    match job.state {
        State::Queued | State::Running => None,
        State::Failed => Some(error(
            StatusCode::UNPROCESSABLE_ENTITY,
            job.error.as_deref().unwrap_or("failed"),
        )),
        State::Done => {
            let Some((record, bytes)) = &job.result else {
                return Some(error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "a done job with no result",
                ));
            };
            let head = serde_json::to_string(record).map(|s| ascii(&s));
            let head = match head.map(HeaderValue::try_from) {
                Ok(Ok(h)) => h,
                _ => {
                    return Some(error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "an unsendable record",
                    ));
                }
            };
            let mut r = Response::new(Body::from(bytes.clone()));
            let h = r.headers_mut();
            let _ = h.insert(RECORD_HEADER, head);
            let _ = h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(match record.ext.as_deref() {
                    Some("avif") => "image/avif",
                    Some("jxl") => "image/jxl",
                    _ => "application/octet-stream",
                }),
            );
            Some(r)
        }
    }
}

/// JSON with every non-ASCII character escaped, so it fits a header.
fn ascii(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut u = [0u16; 2];
            for unit in c.encode_utf16(&mut u) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

async fn forget(Shared(app): Shared<Arc<App>>, Path(key): Path<String>) -> Response {
    let gone = app.lock().remove(&key).is_some();
    if gone {
        app.bump();
        StatusCode::NO_CONTENT.into_response()
    } else {
        error(StatusCode::NOT_FOUND, "no such job")
    }
}

async fn status(Shared(app): Shared<Arc<App>>) -> Response {
    let jobs = app.lock();
    let count = |s| jobs.jobs.values().filter(|j| j.state == s).count();
    let s = Status {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        protocol: PROTOCOL,
        metric: jobs.metric.clone(),
        workers: jobs.workers,
        queued: count(State::Queued),
        running: count(State::Running),
        done: count(State::Done) + count(State::Failed),
        held_bytes: jobs.held,
        max_held_bytes: app.cfg.max_held,
        max_job_bytes: app.cfg.max_job,
        max_jobs: app.cfg.max_jobs,
        result_ttl_s: app.cfg.result_ttl.as_secs(),
        finished_total: jobs.finished_total,
        failed_total: jobs.failed_total,
        cpu_s_total: jobs.cpu_s_total,
        uptime_s: app.started.elapsed().as_secs(),
    };
    drop(jobs);
    axum::Json(s).into_response()
}

/// Drops results nobody fetched within the TTL.
async fn reap(app: Arc<App>) {
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    loop {
        let _ = tick.tick().await;
        let ttl = app.cfg.result_ttl;
        let mut jobs = app.lock();
        let old: Vec<String> = jobs
            .jobs
            .iter()
            .filter(|(_, j)| j.finished.is_some_and(|t| t.elapsed() > ttl))
            .map(|(k, _)| k.clone())
            .collect();
        for k in &old {
            let _ = jobs.remove(k);
        }
        drop(jobs);
        if !old.is_empty() {
            log!("reaped {} results nobody fetched", old.len());
        }
    }
}

async fn accept_workers(listener: UnixListener, app: Arc<App>) {
    let mut next_id = 0u64;
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                next_id += 1;
                let id = next_id;
                let app = Arc::clone(&app);
                drop(tokio::spawn(async move {
                    app.lock().workers += 1;
                    let r = worker(stream, id, &app).await;
                    let mut jobs = app.lock();
                    jobs.workers = jobs.workers.saturating_sub(1);
                    jobs.requeue(id);
                    drop(jobs);
                    app.bump();
                    if let Err(e) = r
                        && e.kind() != io::ErrorKind::UnexpectedEof
                    {
                        log!("worker {id}: {e}");
                    }
                }));
            }
            Err(e) => {
                log!("worker socket: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// Serves one worker connection: waits for `Next`, hands over a job, takes back its result.
async fn worker(stream: UnixStream, id: u64, app: &App) -> io::Result<()> {
    let (r, w) = stream.into_split();
    let mut r = BufReader::new(r);
    let mut w = BufWriter::new(w);
    // a result may be bigger than its source (a kept source is the same size)
    let max_body = app.cfg.max_job.saturating_mul(4);
    loop {
        let (msg, body): (ToServe, Vec<u8>) = read_frame(&mut r, max_body).await?;
        match msg {
            ToServe::Next { metric } => {
                app.lock().metric = metric;
                let (key, settings, source) = take(app, id, &mut r).await?;
                write_frame(&mut w, &ToWork::Job { key, settings }, &source).await?;
            }
            ToServe::Done { key, record } => {
                let mut jobs = app.lock();
                jobs.finished_total += 1;
                jobs.cpu_s_total += record.cpu_s;
                let size = u64::try_from(body.len()).unwrap_or(u64::MAX);
                let mut held = 0;
                if let Some(job) = jobs.jobs.get_mut(&key).filter(|j| j.owner == Some(id)) {
                    held = job.held();
                    job.source = None;
                    job.state = State::Done;
                    job.owner = None;
                    job.result = Some((record, Bytes::from(body)));
                    job.finished = Some(Instant::now());
                }
                // the job may have been deleted while it ran; then its result is dropped
                if held > 0 {
                    jobs.held = jobs.held.saturating_sub(held).saturating_add(size);
                }
                drop(jobs);
                app.bump();
            }
            ToServe::Failed { key, error } => {
                let mut jobs = app.lock();
                jobs.failed_total += 1;
                let mut held = 0;
                if let Some(job) = jobs.jobs.get_mut(&key).filter(|j| j.owner == Some(id)) {
                    held = job.held();
                    job.source = None;
                    job.state = State::Failed;
                    job.owner = None;
                    job.error = Some(error.clone());
                    job.finished = Some(Instant::now());
                }
                jobs.held = jobs.held.saturating_sub(held);
                drop(jobs);
                app.bump();
                log!("job {key} failed: {error}");
            }
        }
    }
}

/// Waits for a queued job and marks it running for connection `id`. Fails if the worker goes
/// away meanwhile, so its place isn't held by a dead connection.
async fn take(
    app: &App,
    id: u64,
    r: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> io::Result<(String, Config, Bytes)> {
    let mut changed = app.changed.subscribe();
    loop {
        let _ = changed.borrow_and_update();
        {
            let mut jobs = app.lock();
            while let Some(key) = jobs.queue.pop_front() {
                if let Some(job) = jobs.jobs.get_mut(&key)
                    && let Some(source) = job.source.clone()
                {
                    job.state = State::Running;
                    job.owner = Some(id);
                    return Ok((key, job.settings.clone(), source));
                }
            }
        }
        // a worker says nothing while it waits, so a read that returns means it closed
        tokio::select! {
            r = changed.changed() => r.map_err(io::Error::other)?,
            b = r.fill_buf() => {
                return Err(match b {
                    Ok([]) => io::Error::new(io::ErrorKind::UnexpectedEof, "the worker left"),
                    Ok(_) => io::Error::new(io::ErrorKind::InvalidData, "a worker spoke out of turn"),
                    Err(e) => e,
                });
            }
        }
    }
}

async fn read_frame<R: AsyncReadExt + Unpin, H: serde::de::DeserializeOwned>(
    r: &mut R,
    max_body: u64,
) -> io::Result<(H, Vec<u8>)> {
    let len = r.read_u32().await?;
    if len > MAX_HEADER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame header too long",
        ));
    }
    let mut head = vec![0; usize::try_from(len).map_err(io::Error::other)?];
    let _ = r.read_exact(&mut head).await?;
    let header =
        serde_json::from_slice(&head).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let body_len = r.read_u64().await?;
    if body_len > max_body {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame body too long",
        ));
    }
    let mut body = vec![0; usize::try_from(body_len).map_err(io::Error::other)?];
    let _ = r.read_exact(&mut body).await?;
    Ok((header, body))
}

async fn write_frame<W: AsyncWriteExt + Unpin, H: serde::Serialize>(
    w: &mut W,
    header: &H,
    body: &[u8],
) -> io::Result<()> {
    let mut head = Vec::new();
    incre_image_compression_proto::frame::write(&mut head, header, &[])?;
    // the sync writer framed the header with an empty body; swap in the real body length
    head.truncate(head.len().saturating_sub(8));
    w.write_all(&head).await?;
    w.write_u64(u64::try_from(body.len()).map_err(io::Error::other)?)
        .await?;
    w.write_all(body).await?;
    w.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[expect(clippy::disallowed_macros, reason = "tests assert")]
    fn headers_stay_ascii() {
        let escaped = ascii("{\"note\":\"\u{e9}\u{1f600}\"}");
        assert_eq!(escaped, r#"{"note":"\u00e9\ud83d\ude00"}"#);
        // and JSON reads it back
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&escaped)
                .ok()
                .and_then(|v| v["note"].as_str().map(str::to_owned))
                .as_deref(),
            Some("\u{e9}\u{1f600}")
        );
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"ab"));
    }
}
