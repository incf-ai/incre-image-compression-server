//! `incre-compress`: sends pictures to a `incre-image-compression-server` and writes what comes back.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};

use incre_image_compression_client::{Client, Config};

const HELP: &str = "\
incre-compress: recompresses pictures on an incre-image-compression-server

usage:
  incre-compress [options] PICTURE... --out DIR
  incre-compress [options] status

Each PICTURE becomes DIR/NAME.avif or DIR/NAME.jxl (or a copy, when nothing beat it) and
DIR/NAME.json, its record. Pictures already done (their .json exists) are skipped.

options:
  --server URL       the server, like http://buildbox:8790 (or INCRE_COMPRESS_URL)
  --token-file PATH  a file holding the token (or INCRE_COMPRESS_TOKEN_FILE;
                     INCRE_COMPRESS_TOKEN holds the token itself)
  --settings JSON    encoding settings; missing fields take the defaults
  --jobs N           pictures in flight at once (default 32)
";

struct Args {
    server: String,
    token: String,
    settings: Config,
    jobs: usize,
    out: Option<PathBuf>,
    inputs: Vec<PathBuf>,
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            let _ignored = writeln!(io::stderr().lock(), "incre-compress: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse() -> io::Result<Option<Args>> {
    let mut server = std::env::var("INCRE_COMPRESS_URL").ok();
    let mut token = std::env::var("INCRE_COMPRESS_TOKEN").ok();
    let mut token_file = std::env::var("INCRE_COMPRESS_TOKEN_FILE")
        .ok()
        .filter(|f| !f.is_empty() && token.is_none());
    let mut settings = Config::default();
    let mut jobs = 32;
    let mut out = None;
    let mut inputs = Vec::new();
    let mut it = std::env::args().skip(1);
    let bad = |m: String| io::Error::new(io::ErrorKind::InvalidInput, m);
    while let Some(a) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .ok_or_else(|| bad(format!("{name} needs a value")))
        };
        match a.as_str() {
            "-h" | "--help" | "help" => {
                writeln!(io::stdout().lock(), "{}", HELP.trim_end())?;
                return Ok(None);
            }
            "--server" => server = Some(value("--server")?),
            "--token-file" => token_file = Some(value("--token-file")?),
            "--settings" => {
                settings = serde_json::from_str(&value("--settings")?)
                    .map_err(|e| bad(format!("--settings: {e}")))?;
            }
            "--jobs" => {
                jobs = value("--jobs")?
                    .parse()
                    .map_err(|_| bad("--jobs: not a number".to_owned()))?;
            }
            "--out" => out = Some(PathBuf::from(value("--out")?)),
            s if s.starts_with("--") => return Err(bad(format!("unknown flag {s}"))),
            _ => inputs.push(PathBuf::from(a)),
        }
    }
    if let Some(f) = token_file {
        token = Some(std::fs::read_to_string(&f).map_err(|e| bad(format!("{f}: {e}")))?);
    }
    let server = server.ok_or_else(|| bad("no --server (or INCRE_COMPRESS_URL)".to_owned()))?;
    let token = token.ok_or_else(|| {
        bad("no --token-file (or INCRE_COMPRESS_TOKEN_FILE or INCRE_COMPRESS_TOKEN)".to_owned())
    })?;
    Ok(Some(Args {
        server,
        token,
        settings,
        jobs: jobs.max(1),
        out,
        inputs,
    }))
}

fn run() -> io::Result<bool> {
    let Some(args) = parse()? else {
        return Ok(true);
    };
    let client = Client::new(&args.server, &args.token);
    if args.inputs.len() == 1
        && args
            .inputs
            .first()
            .is_some_and(|p| p.as_os_str() == "status")
    {
        let s = client.status().map_err(io::Error::other)?;
        let s = serde_json::to_string_pretty(&s).map_err(io::Error::other)?;
        writeln!(io::stdout().lock(), "{s}")?;
        return Ok(true);
    }
    let out = args
        .out
        .clone()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no --out DIR"))?;
    std::fs::create_dir_all(&out)?;
    let next = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..args.jobs.min(args.inputs.len()) {
            let _ = s.spawn(|| {
                while let Some(input) = args.inputs.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let line = match one(&client, &args.settings, input, &out) {
                        Ok(line) => line,
                        Err(e) => {
                            let _ = failed.fetch_add(1, Ordering::Relaxed);
                            format!("{}: {e}", input.display())
                        }
                    };
                    // a closed stdout doesn't stop the work
                    let _ignored = writeln!(io::stdout().lock(), "{line}");
                }
            });
        }
    });
    Ok(failed.load(Ordering::Relaxed) == 0)
}

/// Encodes one picture into `out`; returns a line saying what it became.
fn one(client: &Client, settings: &Config, input: &Path, out: &Path) -> io::Result<String> {
    let stem = input
        .file_stem()
        .ok_or_else(|| io::Error::other("no file name"))?;
    let record_path = out.join(stem).with_extension("json");
    if record_path.exists() {
        return Ok(format!("{}: done already", input.display()));
    }
    let bytes = std::fs::read(input)?;
    let (record, body) = client.encode(&bytes, settings).map_err(io::Error::other)?;
    let name = match &record.ext {
        Some(ext) => out.join(stem).with_extension(ext),
        None => out.join(input.file_name().unwrap_or(stem)),
    };
    write_atomic(&name, &body)?;
    let json = serde_json::to_vec_pretty(&record).map_err(io::Error::other)?;
    write_atomic(&record_path, &json)?;
    Ok(format!(
        "{}: {} {} -> {} bytes ({:.0}%), {:.1} cpu-s",
        input.display(),
        record.codec,
        record.source_size,
        record.size,
        percent(record.size, record.source_size),
        record.cpu_s
    ))
}

fn percent(a: u64, b: u64) -> f64 {
    let f = |n: u64| u32::try_from(n >> 10).map_or(f64::MAX, f64::from);
    if b == 0 {
        0.0
    } else {
        100.0 * f(a) / f(b).max(1.0)
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}
