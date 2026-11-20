# incre-image-compression-server

A picture recompression server that runs on a machine's spare CPU time. Send it a JPEG, PNG,
WebP, GIF or BMP and it returns the smallest AVIF or JPEG XL that still passes a quality gate
(SSIMULACRA2, mean and worst tile), or the original if nothing beats it.

It is built for a box that has other work to do, like a build server: the encoders run at idle
priority, so they get only the CPU time nothing else wants. When the box is busy they stop; the
HTTP side keeps answering, and clients wait as long as it takes.

- **Idle priority.** The encoders run in their own cgroup with `CPUWeight=idle` (`cpu.idle`),
  their threads are `SCHED_IDLE`, at nice 19, with idle-class I/O.
- **RAM only.** Jobs and results are held in memory and never written to disk, so there is no
  SSD wear. A restart drops them, and clients resubmit.
- **Patient clients.** A job is a PUT followed by long-poll GETs. Keys come from the picture's
  bytes and settings, so resubmitting is always safe. The bundled client and the `incre-compress` CLI
  ride out restarts, outages and full queues.
- **Token auth for a LAN.** Plain HTTP with a bearer token. Don't expose it to the internet.

## Install

On the server (x86-64 Linux with systemd, glibc 2.35 or later, a Haswell or newer CPU):

```sh
curl -LO https://github.com/incf-ai/incre-image-compression-server/releases/latest/download/incre-image-compression-server-VERSION-x86_64-linux.tar.gz
tar xzf incre-image-compression-server-*-x86_64-linux.tar.gz
cd incre-image-compression-server-*-x86_64-linux
sudo ./install.sh
```

`install.sh` does the following:

1. Puts `incre-image-compression-server` and `incre-compress` in `/usr/local/bin` (set `PREFIX` to change that).
2. Creates a `incre-image-compression-server` system user.
3. Generates a token in `/etc/incre-image-compression-server/token`.
4. Installs and starts `incre-image-compression-server.service` and `incre-image-compression-worker.service`.

Run it again from a newer tarball to upgrade. It keeps the token and settings.
`sudo ./uninstall.sh [--purge]` removes everything.

Settings live in `/etc/incre-image-compression-server/incre-image-compression-server.env`; the file lists each setting with its default.
Open port 8790 to your LAN only. For example:

```sh
sudo ufw allow from 192.168.0.0/16 to any port 8790 proto tcp
```

## Use

Copy the token to the client machine. Then:

```sh
export INCRE_COMPRESS_URL=http://buildbox:8790 INCRE_COMPRESS_TOKEN_FILE=~/.config/incre-compress-token
incre-compress status
incre-compress --jobs 32 pictures/*.jpg --out smaller/
```

For each picture, `incre-compress` writes `NAME.avif` or `NAME.jxl` into `--out`. If nothing beat the
original, it writes a copy of the source instead. It also writes `NAME.json`, the record: codec,
quality parameter, scores, sizes, sha256, and CPU and wall time.

It skips pictures whose `.json` already exists, so an interrupted run picks up where it stopped.
`--settings '{"speed":8}'` trades a little size for speed; the fields are `incre-image-compression-proto`'s `Config`.

From Rust, use the `incre-image-compression-client` crate:

```rust
let client = incre_image_compression_client::Client::new("http://buildbox:8790", &token);
let (record, bytes) = client.encode(&picture, &incre_image_compression_client::Config::default())?;
```

## How it works

```
 clients ──HTTP──▶ incre-image-compression-server serve ──unix socket──▶ incre-image-compression-server work
                   normal priority                   idle cgroup, SCHED_IDLE, nice 19
                   queue + results in RAM            one job per thread
```

Linux CPU idleness applies to a cgroup as a whole, so one process can't hold both responsive
network threads and idle workers. That is why the server is two processes, started from one
binary:

- **`serve`** answers HTTP, holds the queue and results, and runs at normal priority. It sleeps
  nearly all the time.
- **`work`** runs one thread per CPU. Each thread connects to `serve` over
  `/run/incre-image-compression-server/work.sock` and blocks until it is handed a job.
  The threads share a memory budget, 80% of the unit's `MemoryHigh` by default (`--memory-mb`
  to set it): before a picture is encoded its thread reserves an estimate from the picture's
  header, and waits while the pictures in progress hold too much. Otherwise enough threads
  with big pictures at once push the cgroup past `MemoryHigh`, and the kernel stalls them all
  in reclaim.

If a worker dies mid-job, its job goes back to the front of the queue. If `serve` restarts, the
workers reconnect.

### HTTP API

Every call except `/healthz` needs `Authorization: Bearer TOKEN`.

| Call | Answers |
|---|---|
| `PUT /v1/jobs/{key}`<br>body: the picture<br>`x-incre-compress-settings`: settings JSON | `202` queued; `200` already known (both with `{key,state,position}`); `503` full, with `Retry-After`; `413` too large |
| `GET /v1/jobs/{key}?wait=S` | `200` result as the body, with the record in `x-incre-compress-record`; `202` still pending (after waiting up to `S` ≤ 120 seconds); `422` the encoder failed; `404` unknown |
| `DELETE /v1/jobs/{key}` | `204`; `404` |
| `GET /v1/status` | queue, worker, memory and total counts |
| `GET /healthz` | `200 ok` |

Keys are 1–128 characters from `A-Za-z0-9._-`. The client uses the first 40 hex digits of
sha256(picture ‖ settings).

The server deletes a result once its TTL passes (30 minutes by default). Fetch the result and
then delete it. When a key comes back unknown (`404`), resubmit it.

The server holds at most 2 GB of pictures and results by default, and at most 10,000 jobs. Past
either limit, a PUT gets `503` and should be retried after the time in `Retry-After`.

### Long waits

On a busy box a job can wait a long time, and every part is built to tolerate that:

- **Fetches** are long-polls of up to two minutes. The client repeats them, and they count as
  progress, not as trouble.
- **Client timeouts** are generous: 30 s to connect and 10 minutes to send or receive a body.
  Idle connections are dropped after 50 s, before a NAT or proxy might cut them.
- **Retries:** the client backs off from 1 s up to 60 s on errors and full queues. It gives up
  only after `patience` (6 h by default) of continuous trouble. Time spent queued never counts.

## Build

```sh
sudo apt install cmake nasm meson ninja-build   # for libaom, dav1d, libavif, libjxl
cargo build --release
cargo test --workspace --release
```

Running the server by hand, without systemd:

```sh
INCRE_COMPRESS_TOKEN=$(incre-image-compression-server token) incre-image-compression-server serve --socket /tmp/rw.sock &
systemd-run --user --scope -p CPUWeight=idle incre-image-compression-server work --socket /tmp/rw.sock
```

## Licence

MIT. The binaries bundle libavif, libaom, dav1d, libjxl and their dependencies; see
[THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md) and the tarball's `licenses/` folder.
