//! `incre-image-compression-server`: a picture recompression server that lives on a machine's spare CPU time.
//!
//! It runs as two processes, because Linux's CPU idleness is relative to siblings: a process
//! can't hold both responsive I/O threads and idle-priority workers.
//!
//! - `incre-image-compression-server serve` answers HTTP, checks the token and holds the queue and results in RAM.
//!   It runs at normal priority but sleeps nearly always, so a busy machine still answers.
//! - `incre-image-compression-server work` encodes. It runs in an idle cgroup (`CPUWeight=idle`) at nice 19, its
//!   threads `SCHED_IDLE`, and takes jobs from `serve` over a local unix socket.
//!
//! Settings come from flags or from `INCRE_COMPRESS_*` variables (see `incre-image-compression-server help`).

mod config;
mod serve;
mod work;

use std::io::{self, Write};
use std::process::ExitCode;

/// Writes a line to stderr (journald keeps it); a failed write is dropped.
macro_rules! log {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        let _ignored = writeln!(std::io::stderr().lock(), $($t)*);
    }};
}
pub(crate) use log;

const HELP: &str = "\
incre-image-compression-server: recompresses pictures to AVIF or JPEG XL on spare CPU time

usage:
  incre-image-compression-server serve [--listen ADDR] [--token-file PATH] [--socket PATH]
      [--max-held-mb N] [--max-job-mb N] [--max-jobs N] [--result-ttl-s N]
  incre-image-compression-server work [--socket PATH] [--threads N] [--memory-mb N]
      [--no-sched-idle]
  incre-image-compression-server token      print a new random token
  incre-image-compression-server version | help

Each flag can be set with its INCRE_COMPRESS_ variable instead (--max-held-mb is
INCRE_COMPRESS_MAX_HELD_MB); a flag wins. Defaults:

  --listen        0.0.0.0:8790
  --token-file    /etc/incre-image-compression-server/token
                  (INCRE_COMPRESS_TOKEN holds the token itself)
  --socket        /run/incre-image-compression-server/work.sock
  --max-held-mb   2048   RAM for sources waiting and results not yet fetched
  --max-job-mb    64     the largest picture taken
  --max-jobs      10000
  --result-ttl-s  1800   results nobody fetches and deletes go after this
  --threads       every CPU
  --memory-mb     80% of the cgroup's MemoryHigh (else MemoryMax, else half the RAM):
                  the memory the jobs being encoded may take at once; a thread waits
                  for room before starting a picture, from an estimate of its size
";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    let rest: Vec<String> = args.collect();
    let result = match command.as_str() {
        "serve" => config::Serve::parse(&rest).and_then(serve::run),
        "work" => config::Work::parse(&rest).and_then(work::run),
        "token" => token().and_then(|t| say(&t)),
        "version" | "--version" | "-V" => say(env!("CARGO_PKG_VERSION")),
        "help" | "--help" | "-h" => say(HELP.trim_end()),
        "" => Err(io::Error::other(
            "no command; see `incre-image-compression-server help`",
        )),
        other => Err(io::Error::other(format!(
            "unknown command {other:?}; see `incre-image-compression-server help`"
        ))),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log!("incre-image-compression-server: {e}");
            ExitCode::FAILURE
        }
    }
}

fn say(line: &str) -> io::Result<()> {
    writeln!(io::stdout().lock(), "{line}")
}

/// 32 random bytes from the kernel, in hex.
fn token() -> io::Result<String> {
    let mut b = [0u8; 32];
    io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}
