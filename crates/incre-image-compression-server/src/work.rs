//! `incre-image-compression-server work`: worker threads at idle priority, each taking one job at a time from
//! `serve` over the unix socket.
//!
//! The process is meant to run in an idle cgroup at nice 19 (the systemd unit sets both); each
//! thread also asks for `SCHED_IDLE`, so it only gets CPU time nothing else wants. A thread
//! blocks on its socket until `serve` hands it a job, however long that is; if `serve` goes
//! away it reconnects every few seconds.

use std::io::{self, BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::time::{Duration, Instant};

use incre_image_compression_core::{Choice, Chooser, Cpu, Metric};
use incre_image_compression_proto::frame::{self, ToServe, ToWork};
use incre_image_compression_proto::{Config, Record};
use sha2::{Digest, Sha256};

use crate::config::Work;
use crate::log;

/// The largest job body taken from `serve` (it enforces its own, smaller limit).
const MAX_BODY: u64 = 1 << 30;

pub fn run(cfg: Work) -> io::Result<()> {
    log!(
        "incre-image-compression-server work {}: {} threads on {}, SCHED_IDLE {}",
        env!("CARGO_PKG_VERSION"),
        cfg.threads,
        cfg.socket.display(),
        if cfg.sched_idle { "on" } else { "off" }
    );
    std::thread::scope(|s| {
        for n in 0..cfg.threads {
            let cfg = &cfg;
            if let Err(e) = std::thread::Builder::new()
                .name(format!("work-{n}"))
                .spawn_scoped(s, move || thread(n, cfg))
            {
                log!("work-{n}: can't start: {e}");
            }
        }
    });
    Ok(())
}

fn thread(n: usize, cfg: &Work) {
    if cfg.sched_idle
        && let Err(e) = sched_idle()
    {
        log!("work-{n}: SCHED_IDLE refused ({e}); running at the process's priority");
    }
    let mut said = false;
    loop {
        match connection(&cfg.socket) {
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
fn connection(socket: &Path) -> io::Result<()> {
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
        match encode(&settings, &body, &metric) {
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
}
