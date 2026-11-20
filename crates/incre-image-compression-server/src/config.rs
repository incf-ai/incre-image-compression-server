//! The two commands' settings, from flags or `INCRE_COMPRESS_*` variables.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Flags and their values: `--name value`, or `--name` alone for a switch.
struct Flags<'a> {
    pairs: Vec<(&'a str, Option<&'a str>)>,
}

impl<'a> Flags<'a> {
    fn parse(args: &'a [String], known: &[&str], switches: &[&str]) -> io::Result<Self> {
        let mut pairs = Vec::new();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let name = a
                .strip_prefix("--")
                .ok_or_else(|| io::Error::other(format!("unexpected argument {a:?}")))?;
            if switches.contains(&name) {
                pairs.push((name, None));
            } else if known.contains(&name) {
                let v = it
                    .next()
                    .ok_or_else(|| io::Error::other(format!("--{name} needs a value")))?;
                pairs.push((name, Some(v.as_str())));
            } else {
                return Err(io::Error::other(format!("unknown flag --{name}")));
            }
        }
        Ok(Self { pairs })
    }

    /// The flag's value, else its variable's (`--max-jobs` is `INCRE_COMPRESS_MAX_JOBS`).
    fn get(&self, name: &str) -> Option<String> {
        self.pairs
            .iter()
            .rev()
            .find(|(n, _)| *n == name)
            .and_then(|(_, v)| v.map(str::to_owned))
            .or_else(|| std::env::var(env_name(name)).ok())
            .filter(|v| !v.is_empty())
    }

    fn switch(&self, name: &str) -> bool {
        self.pairs.iter().any(|(n, _)| *n == name)
            || std::env::var(env_name(name))
                .is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes"))
    }

    fn num<T: std::str::FromStr>(&self, name: &str, default: T) -> io::Result<T> {
        match self.get(name) {
            None => Ok(default),
            Some(v) => v
                .trim()
                .parse()
                .map_err(|_| io::Error::other(format!("--{name}: not a number: {v:?}"))),
        }
    }
}

fn env_name(flag: &str) -> String {
    format!("INCRE_COMPRESS_{}", flag.replace('-', "_").to_uppercase())
}

const DEFAULT_SOCKET: &str = "/run/incre-image-compression-server/work.sock";

/// `incre-image-compression-server serve`.
#[derive(Debug, Clone)]
pub struct Serve {
    pub listen: SocketAddr,
    pub token: String,
    pub socket: PathBuf,
    pub max_held: u64,
    pub max_job: u64,
    pub max_jobs: usize,
    pub result_ttl: Duration,
}

impl Serve {
    pub fn parse(args: &[String]) -> io::Result<Self> {
        let f = Flags::parse(
            args,
            &[
                "listen",
                "token-file",
                "socket",
                "max-held-mb",
                "max-job-mb",
                "max-jobs",
                "result-ttl-s",
            ],
            &[],
        )?;
        let listen = f.get("listen").unwrap_or_else(|| "0.0.0.0:8790".to_owned());
        let listen = listen
            .parse()
            .map_err(|_| io::Error::other(format!("--listen: not an address: {listen:?}")))?;
        let token = match std::env::var("INCRE_COMPRESS_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
        {
            Some(t) => t,
            None => {
                let path = f
                    .get("token-file")
                    .unwrap_or_else(|| "/etc/incre-image-compression-server/token".to_owned());
                std::fs::read_to_string(&path)
                    .map_err(|e| io::Error::new(e.kind(), format!("token file {path}: {e}")))?
            }
        };
        let token = token.trim().to_owned();
        if token.len() < 16 {
            return Err(io::Error::other(
                "the token is under 16 characters; make one with `incre-image-compression-server token`",
            ));
        }
        let mb = 1 << 20;
        Ok(Self {
            listen,
            token,
            socket: f
                .get("socket")
                .unwrap_or_else(|| DEFAULT_SOCKET.to_owned())
                .into(),
            max_held: f.num("max-held-mb", 2048u64)?.saturating_mul(mb),
            max_job: f.num("max-job-mb", 64u64)?.saturating_mul(mb),
            max_jobs: f.num("max-jobs", 10_000usize)?.max(1),
            result_ttl: Duration::from_secs(f.num("result-ttl-s", 1800u64)?.max(10)),
        })
    }
}

/// `incre-image-compression-server work`.
#[derive(Debug, Clone)]
pub struct Work {
    pub socket: PathBuf,
    pub threads: usize,
    pub sched_idle: bool,
    /// The memory the threads' jobs may hold at once, in bytes; 0 picks it from the cgroup's
    /// limit (see `work::budget`).
    pub memory: u64,
}

impl Work {
    pub fn parse(args: &[String]) -> io::Result<Self> {
        let f = Flags::parse(
            args,
            &["socket", "threads", "memory-mb"],
            &["no-sched-idle"],
        )?;
        let all = std::thread::available_parallelism().map_or(1, usize::from);
        Ok(Self {
            socket: f
                .get("socket")
                .unwrap_or_else(|| DEFAULT_SOCKET.to_owned())
                .into(),
            threads: match f.num("threads", 0usize)? {
                0 => all,
                n => n,
            },
            sched_idle: !f.switch("no-sched-idle"),
            memory: f.num("memory-mb", 0u64)?.saturating_mul(1 << 20),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[expect(clippy::disallowed_macros, reason = "tests assert")]
    fn flags_parse() -> io::Result<()> {
        let args: Vec<String> = [
            "--socket",
            "/tmp/s",
            "--threads",
            "3",
            "--no-sched-idle",
            "--memory-mb",
            "100",
        ]
        .map(String::from)
        .into();
        let w = Work::parse(&args)?;
        assert_eq!(w.socket, PathBuf::from("/tmp/s"));
        assert_eq!(w.threads, 3);
        assert_eq!(w.memory, 100 << 20);
        assert!(!w.sched_idle);
        assert!(Work::parse(&["--threads".to_owned()]).is_err());
        assert!(Work::parse(&["--bogus".to_owned(), "1".to_owned()]).is_err());
        assert!(Work::parse(&["--threads".to_owned(), "x".to_owned()]).is_err());
        assert_eq!(env_name("max-held-mb"), "INCRE_COMPRESS_MAX_HELD_MB");
        Ok(())
    }
}
