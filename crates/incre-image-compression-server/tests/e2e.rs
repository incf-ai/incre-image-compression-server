//! Runs `incre-image-compression-server serve` and `incre-image-compression-server work` as processes and drives them with the client.

#![expect(clippy::disallowed_macros, reason = "tests assert")]

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use incre_image_compression_client::{Client, Config, Error, Fetched, Submitted, job_key};

type R = Result<(), Box<dyn std::error::Error>>;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _killed = self.0.kill();
        let _reaped = self.0.wait();
    }
}

struct Server {
    url: String,
    socket: PathBuf,
    port: u16,
    serve: Option<Proc>,
    dir: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        drop(self.serve.take());
        let _removed = std::fs::remove_dir_all(&self.dir);
    }
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_incre-image-compression-server"))
}

fn free_port() -> std::io::Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

impl Server {
    fn start(name: &str) -> Result<Self, Box<dyn std::error::Error>> {
        // A unix socket's path must fit in 108 bytes, which CARGO_TARGET_TMPDIR may not
        // (a GitHub checkout of this repo is 90 bytes deep), so the socket goes in /tmp.
        let dir = std::env::temp_dir().join(format!("icss-{}-{name}", std::process::id()));
        let _stale = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let port = free_port()?;
        let mut s = Self {
            url: format!("http://127.0.0.1:{port}"),
            socket: dir.join("work.sock"),
            port,
            serve: None,
            dir,
        };
        s.restart()?;
        Ok(s)
    }

    fn restart(&mut self) -> R {
        drop(self.serve.take());
        let child = bin()
            .args(["serve", "--listen", &format!("127.0.0.1:{}", self.port)])
            .arg("--socket")
            .arg(&self.socket)
            .args(["--max-held-mb", "8", "--max-jobs", "3"])
            .env("INCRE_COMPRESS_TOKEN", TOKEN)
            .stderr(std::fs::File::create(self.dir.join("serve.log"))?)
            .spawn()?;
        self.serve = Some(Proc(child));
        let client = Client::new(&self.url, TOKEN);
        let t = Instant::now();
        while client.status().is_err() {
            if t.elapsed() > Duration::from_secs(20) {
                let log = std::fs::read_to_string(self.dir.join("serve.log")).unwrap_or_default();
                return Err(format!("serve didn't start: {log}").into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }

    fn work(&self, threads: usize) -> Result<Proc, Box<dyn std::error::Error>> {
        let child = bin()
            .arg("work")
            .arg("--socket")
            .arg(&self.socket)
            .args(["--threads", &threads.to_string()])
            .stderr(std::fs::File::create(self.dir.join("work.log"))?)
            .spawn()?;
        Ok(Proc(child))
    }
}

/// A noisy gradient PNG, so the lossy codecs have something to do.
fn png(w: u32, h: u32) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut seed = 1u32;
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
        let n = u8::try_from((seed >> 24) & 15).unwrap_or(0);
        let r = u8::try_from(x * 255 / w).unwrap_or(0);
        let g = u8::try_from(y * 255 / h).unwrap_or(0);
        image::Rgb([r.saturating_add(n), g.saturating_add(n), 128])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

fn fast() -> Config {
    Config {
        speed: 9,
        ..Config::default()
    }
}

#[test]
fn a_picture_round_trips() -> R {
    let server = Server::start("round-trip")?;
    let _work = server.work(2)?;
    let client = Client::new(&server.url, TOKEN);
    let picture = png(320, 240)?;
    let (record, body) = client.encode(&picture, &fast())?;
    assert!(record.size < record.source_size, "{record:?}");
    assert_eq!(record.size, u64::try_from(body.len())?);
    assert!(matches!(
        record.codec.as_str(),
        "avif" | "jxl-lossy" | "jxl-lossless"
    ));
    assert_eq!(record.width, Some(320));
    // fetched results are deleted
    assert_eq!(
        client.fetch(&job_key(&picture, &fast()), Duration::ZERO)?,
        Fetched::Unknown
    );
    let status = client.status()?;
    assert_eq!(status.workers, 2);
    assert_eq!(status.finished_total, 1);
    assert_eq!(status.held_bytes, 0);
    assert_eq!(status.metric, "ssimulacra2 crate (cpu)");
    // junk comes back as itself
    let (record, body) = client.encode(b"not a picture", &fast())?;
    assert_eq!(record.codec, "source");
    assert_eq!(body, b"not a picture");
    Ok(())
}

#[test]
fn the_token_is_checked() -> R {
    let server = Server::start("token")?;
    let bad = Client::new(&server.url, "wrong-token-wrong-token");
    assert!(matches!(bad.status(), Err(Error::Unauthorized)));
    assert!(matches!(
        bad.submit("k", b"x", &fast()),
        Err(Error::Unauthorized)
    ));
    let health = ureq::get(&format!("{}/healthz", server.url)).call()?;
    assert_eq!(health.status(), 200);
    Ok(())
}

#[test]
fn jobs_wait_for_a_worker_and_survive_a_restart() -> R {
    let mut server = Server::start("restart")?;
    let client = Client::new(&server.url, TOKEN);
    let picture = png(64, 64)?;
    let key = job_key(&picture, &fast());
    // no worker yet: the job waits, and a resubmission finds it
    assert!(matches!(
        client.submit(&key, &picture, &fast())?,
        Submitted::Queued(_)
    ));
    assert!(matches!(
        client.submit(&key, &picture, &fast())?,
        Submitted::Queued(_)
    ));
    match client.fetch(&key, Duration::from_secs(1))? {
        Fetched::Pending(info) => assert_eq!(info.position, 0),
        other => return Err(format!("{other:?}").into()),
    }
    // the queue holds three jobs
    assert!(matches!(
        client.submit("b", &picture, &fast())?,
        Submitted::Queued(_)
    ));
    assert!(matches!(
        client.submit("c", &picture, &fast())?,
        Submitted::Queued(_)
    ));
    assert!(matches!(
        client.submit("d", &picture, &fast())?,
        Submitted::Full(_)
    ));
    assert!(client.delete("b")?);
    assert!(client.delete("c")?);
    assert!(!client.delete("c")?);
    // a restart forgets the job
    server.restart()?;
    assert_eq!(client.fetch(&key, Duration::ZERO)?, Fetched::Unknown);
    // encode resubmits it; a worker that starts late (or reconnects) picks it up
    let socket_url = server.url.clone();
    let encoder = std::thread::spawn(move || {
        Client::new(&socket_url, TOKEN).encode(&png(64, 64).unwrap_or_default(), &fast())
    });
    std::thread::sleep(Duration::from_millis(500));
    let _work = server.work(1)?;
    let (record, _) = encoder
        .join()
        .map_err(|_| "the encoder thread panicked")??;
    assert!(record.probes > 0 || record.codec == "source");
    Ok(())
}

#[test]
fn a_worker_dying_mid_job_requeues_it() -> R {
    let server = Server::start("requeue")?;
    let client = Client::new(&server.url, TOKEN);
    let picture = png(1600, 1200)?;
    let key = job_key(&picture, &fast());
    assert!(matches!(
        client.submit(&key, &picture, &fast())?,
        Submitted::Queued(_)
    ));
    let work = server.work(1)?;
    // wait until it runs, then kill the worker
    let t = Instant::now();
    while client.status()?.running == 0 {
        if t.elapsed() > Duration::from_secs(20) {
            return Err("never ran".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(work);
    let t = Instant::now();
    loop {
        let s = client.status()?;
        if s.queued == 1 && s.workers == 0 {
            break;
        }
        if t.elapsed() > Duration::from_secs(20) {
            return Err(format!("not requeued: {s:?}").into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _work = server.work(1)?;
    match client.fetch(&key, Duration::from_secs(110))? {
        Fetched::Done(record, _) => assert_eq!(record.width, Some(1600)),
        other => return Err(format!("{other:?}").into()),
    }
    Ok(())
}
