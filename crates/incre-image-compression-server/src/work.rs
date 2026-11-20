//! `incre-image-compression-server work`: worker threads at idle priority, each taking one job at a time from
//! `serve` over the unix socket.
//!
//! The process is meant to run in an idle cgroup at nice 19 (the systemd unit sets both); each
//! thread also asks for `SCHED_IDLE`, so it only gets CPU time nothing else wants. A thread
//! blocks on its socket until `serve` hands it a job, however long that is; if `serve` goes
//! away it reconnects every few seconds.
//!
//! The threads share a memory budget: before a picture is encoded its thread reserves an
//! estimate of the memory it will take (from the picture's header), and waits while the
//! pictures being encoded hold too much. Without it a cgroup's `MemoryHigh` stalls every
//! thread in reclaim once enough of them have big pictures at once. After each picture the
//! thread hands its freed memory back (`malloc_trim`), so the process doesn't keep every
//! thread's largest picture.

use std::io::{self, BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use incre_image_compression_core::{Choice, Chooser, Cpu, Metric, img};
use incre_image_compression_proto::frame::{self, ToServe, ToWork};
use incre_image_compression_proto::{Config, Record};
use sha2::{Digest, Sha256};

use crate::config::Work;
use crate::log;

/// The largest job body taken from `serve` (it enforces its own, smaller limit).
const MAX_BODY: u64 = 1 << 30;

pub fn run(cfg: Work) -> io::Result<()> {
    let (memory, whence) = if cfg.memory > 0 {
        (cfg.memory, "--memory-mb".to_owned())
    } else {
        budget()
    };
    log!(
        "incre-image-compression-server work {}: {} threads on {}, SCHED_IDLE {}, {} MiB for jobs ({whence})",
        env!("CARGO_PKG_VERSION"),
        cfg.threads,
        cfg.socket.display(),
        if cfg.sched_idle { "on" } else { "off" },
        memory >> 20
    );
    let budget = Budget::new(memory);
    std::thread::scope(|s| {
        for n in 0..cfg.threads {
            let (cfg, budget) = (&cfg, &budget);
            if let Err(e) = std::thread::Builder::new()
                .name(format!("work-{n}"))
                .spawn_scoped(s, move || thread(n, cfg, budget))
            {
                log!("work-{n}: can't start: {e}");
            }
        }
    });
    Ok(())
}

/// The default budget: 80% of the cgroup's `memory.high`, else of its `memory.max`, else of
/// half the RAM; and where it came from.
fn budget() -> (u64, String) {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|c| {
            c.lines()
                .find_map(|l| l.strip_prefix("0::").map(str::to_owned))
        })
        .map(|path| format!("/sys/fs/cgroup{}", path.trim_end()));
    let limit = |file: &str| {
        cgroup
            .as_ref()
            .and_then(|dir| std::fs::read_to_string(format!("{dir}/{file}")).ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
    };
    let ram = || {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|m| {
                m.lines()
                    .find_map(|l| l.strip_prefix("MemTotal:"))
                    .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            })
            .map(|kb| kb * 1024 / 2)
    };
    let (bytes, whence) = if let Some(b) = limit("memory.high") {
        (b, "of MemoryHigh")
    } else if let Some(b) = limit("memory.max") {
        (b, "of MemoryMax")
    } else if let Some(b) = ram() {
        (b, "of half the RAM")
    } else {
        (4 << 30, "of a guessed 4 GiB")
    };
    (bytes / 5 * 4, format!("80% {whence}"))
}

/// Memory the jobs being encoded have reserved, out of a total.
struct Budget {
    total: u64,
    used: Mutex<u64>,
    freed: Condvar,
}

/// A reservation, given back when dropped.
struct Lease<'a> {
    budget: &'a Budget,
    bytes: u64,
}

impl Budget {
    fn new(total: u64) -> Self {
        Self {
            total: total.max(1),
            used: Mutex::new(0),
            freed: Condvar::new(),
        }
    }

    /// Waits until `bytes` fit, then reserves them. A job larger than the whole budget waits
    /// until it can have all of it.
    fn take(&self, bytes: u64) -> Lease<'_> {
        let bytes = bytes.clamp(1, self.total);
        let mut used = self.used.lock().unwrap_or_else(PoisonError::into_inner);
        while *used + bytes > self.total {
            used = self
                .freed
                .wait(used)
                .unwrap_or_else(PoisonError::into_inner);
        }
        *used += bytes;
        Lease {
            budget: self,
            bytes,
        }
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let mut used = self
            .budget
            .used
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *used = used.saturating_sub(self.bytes);
        self.budget.freed.notify_all();
    }
}

/// Roughly the most memory encoding `bytes` takes, from peaks measured on 7–48 MP pictures:
/// 52 bytes for each pixel scored (after any resize), 9 for each source pixel when the
/// picture is resized, twice the file, and 64 MiB besides. A header that can't be read counts
/// as 4000×3000.
fn estimate(settings: &Config, bytes: &[u8]) -> u64 {
    let (w, h) = img::dimensions(bytes).unwrap_or((4000, 3000));
    let source = u64::from(w) * u64::from(h);
    let edge = u64::from(w.min(h)).max(1);
    let short = u64::from(settings.short_edge);
    let resized = f64::from(w.min(h)) > f64::from(settings.short_edge) * settings.short_margin;
    // the area `Img::fit` gives, without its rounding
    let scored = if resized {
        u64::try_from(u128::from(source) * u128::from(short * short) / u128::from(edge * edge))
            .unwrap_or(source)
    } else {
        source
    };
    let file = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    (64 << 20)
        + scored.saturating_mul(52)
        + if resized { source.saturating_mul(9) } else { 0 }
        + file.saturating_mul(2)
}

/// Hands freed heap back to the system.
fn trim() {
    #[cfg(target_env = "gnu")]
    // SAFETY: malloc_trim only releases free memory; it takes no pointers.
    unsafe {
        let _released: libc::c_int = libc::malloc_trim(0);
    }
}

fn thread(n: usize, cfg: &Work, budget: &Budget) {
    if cfg.sched_idle
        && let Err(e) = sched_idle()
    {
        log!("work-{n}: SCHED_IDLE refused ({e}); running at the process's priority");
    }
    let mut said = false;
    loop {
        match connection(&cfg.socket, budget) {
            Ok(()) => said = false,
            Err(e) => {
                // say it once per outage, not every retry
                if !said {
                    log!("work-{n}: {}: {e}; retrying", cfg.socket.display());
                    said = true;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// Puts the calling thread in `SCHED_IDLE`.
fn sched_idle() -> io::Result<()> {
    let param = libc::sched_param { sched_priority: 0 };
    // SAFETY: pid 0 is the calling thread; `param` is a valid sched_param.
    let r = unsafe { libc::sched_setscheduler(0, libc::SCHED_IDLE, &raw const param) };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Serves one connection until it fails.
fn connection(socket: &Path, budget: &Budget) -> io::Result<()> {
    let stream = UnixStream::connect(socket)?;
    let mut r = BufReader::new(stream.try_clone()?);
    let mut w = BufWriter::new(stream);
    let metric = Cpu;
    loop {
        let next = ToServe::Next {
            metric: metric.name().to_owned(),
        };
        frame::write(&mut w, &next, &[])?;
        let (ToWork::Job { key, settings }, body) = frame::read(&mut r, MAX_BODY)?;
        let lease = budget.take(estimate(&settings, &body));
        let result = encode(&settings, &body, &metric);
        drop(lease);
        trim();
        match result {
            Ok((record, out)) => frame::write(&mut w, &ToServe::Done { key, record }, &out)?,
            Err(error) => frame::write(&mut w, &ToServe::Failed { key, error }, &[])?,
        }
    }
}

/// Encodes one picture; a codec panic is caught and becomes the job's error.
fn encode(
    settings: &Config,
    bytes: &[u8],
    metric: &dyn Metric,
) -> Result<(Record, Vec<u8>), String> {
    let chooser = Chooser(settings.clone());
    let wall = Instant::now();
    let cpu = incre_image_compression_core::choose::thread_cpu();
    let choice =
        catch_unwind(AssertUnwindSafe(|| chooser.choose_with(bytes, metric))).map_err(|p| {
            let what = p
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown".to_owned());
            format!("the encoder panicked: {what}")
        })?;
    let cpu_s = incre_image_compression_core::choose::thread_cpu() - cpu;
    let wall_s = wall.elapsed().as_secs_f64();
    Ok(record(choice, bytes.len(), metric.name(), cpu_s, wall_s))
}

fn record(
    c: Choice,
    source_size: usize,
    metric: &str,
    cpu_s: f64,
    wall_s: f64,
) -> (Record, Vec<u8>) {
    let sha256 = Sha256::digest(&c.bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let record = Record {
        codec: c.codec.name().to_owned(),
        ext: c.codec.ext().map(str::to_owned),
        param: c.param,
        mean: c.score.map(|s| s.mean),
        min: c.score.map(|s| s.worst.score),
        width: c.size.map(|s| s.0),
        height: c.size.map(|s| s.1),
        resized: c.resized,
        probes: c.probes,
        note: c.note,
        metric: metric.to_owned(),
        source_size: u64::try_from(source_size).unwrap_or(u64::MAX),
        size: u64::try_from(c.bytes.len()).unwrap_or(u64::MAX),
        sha256,
        cpu_s,
        wall_s,
    };
    (record, c.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[expect(clippy::disallowed_macros, reason = "tests assert")]
    fn junk_is_kept_as_the_source() -> Result<(), String> {
        let (r, out) = encode(&Config::default(), b"not a picture", &Cpu)?;
        assert_eq!(r.codec, "source");
        assert_eq!(out, b"not a picture");
        assert_eq!(r.size, 13);
        assert!(r.note.is_some_and(|n| n.starts_with("decode")));
        Ok(())
    }

    #[test]
    #[expect(clippy::disallowed_macros, reason = "tests assert")]
    fn the_estimate_covers_the_measured_peaks() {
        let png = |w: u32, h: u32| {
            let mut out = Vec::new();
            let img = image::RgbImage::new(w, h);
            img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
                .map(|()| out)
        };
        let mb = |b: u64| b >> 20;
        let c = Config::default();
        // peaks measured with the v0.1.0 worker: 392 MB at 2700×2700 (scored whole),
        // 821 MB at 2700×6000 (whole), 740 MB at 8000×6000 (resized to 2731×2048)
        for (w, h, peak) in [(2700, 2700, 392), (2700, 6000, 821), (8000, 6000, 740)] {
            let e = mb(estimate(&c, &png(w, h).unwrap_or_default()));
            assert!(e >= peak && e < peak * 3 / 2, "{w}×{h}: {e} MB for {peak}");
        }
        let junk = mb(estimate(&c, b"junk"));
        assert!(
            (300..1000).contains(&junk),
            "an unreadable header: {junk} MB"
        );
    }

    #[test]
    #[expect(clippy::disallowed_macros, reason = "tests assert")]
    fn a_lease_waits_for_room() {
        let b = Budget::new(100);
        let big = b.take(1000);
        assert_eq!(big.bytes, 100, "a job over the budget takes all of it");
        drop(big);
        let first = b.take(70);
        std::thread::scope(|s| {
            let waiter = s.spawn(|| b.take(50).bytes);
            std::thread::sleep(Duration::from_millis(50));
            assert!(!waiter.is_finished(), "70 + 50 is over 100");
            drop(first);
            assert_eq!(waiter.join().ok(), Some(50));
        });
        assert_eq!(*b.used.lock().unwrap_or_else(PoisonError::into_inner), 0);
    }

    #[test]
    #[expect(clippy::disallowed_macros, reason = "tests assert")]
    fn the_default_budget_is_found() {
        let (bytes, whence) = budget();
        assert!(
            bytes > 0 && whence.starts_with("80% of"),
            "{bytes} {whence}"
        );
    }
}
