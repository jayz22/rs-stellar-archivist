# Concurrency Tuning: `--max-concurrent` and `--concurrency`

This document explains exactly what the concurrency knobs control, how they
interact, what happens when you change them, and how to choose values that
balance throughput against the health of the storage backend.

## TL;DR

- `--concurrency` (`-c`, default **32**) = how many *checkpoints* are processed
  in parallel.
- `--max-concurrent` (default **64**) = a hard ceiling on simultaneous *I/O
  operations per storage backend*. It is the real throttle for remote work.
- For HTTP backends, `--max-concurrent` is effectively the cap on simultaneous
  TCP connections / in-flight requests to the server, because the HTTP client
  sets no per-host connection limit of its own.
- The optimal value is **not a constant** — it's the knee of a
  throughput-vs-concurrency curve for *your specific mode and backend*, chosen
  to sit just below where the backend starts returning retryable errors.

## The two knobs

| Flag | Default | What it bounds | Defined at |
|------|---------|----------------|------------|
| `-c, --concurrency` | 32 | Checkpoints processed concurrently (`for_each_concurrent`) | `src/cli/mod.rs:56`, `src/pipeline.rs:327` |
| `--max-concurrent` | 64 | Simultaneous I/O ops per backend (OpenDAL `ConcurrentLimitLayer`), applied independently to source and destination | `src/cli/mod.rs:85`, `src/storage.rs:254` |

### Comparison with go-stellar-archivist

The Go reference CLI (`go-stellar-sdk/tools/stellar-archivist/main.go:210-216`)
exposes a **single** concurrency knob, `--concurrency`/`-c`, defaulting to
**32** ("number of files to operate on concurrently"). There is no separate
per-backend I/O limit: each of the 32 worker goroutines does its own sequential
I/O, so `-c` simultaneously caps both work parallelism and in-flight I/O.

The Rust port keeps the `32` default for `--concurrency` (parity with Go) but
adds `--max-concurrent` as a distinct lower layer. This is necessary because the
Rust pipeline does not use a one-I/O-per-worker model — each checkpoint fans out
into *several* concurrent file operations (see below), so the number of in-flight
requests is decoupled from the number of checkpoints.

## What `--max-concurrent` actually sets

The value becomes the permit count of a single `tokio::Semaphore` that OpenDAL's
`ConcurrentLimitLayer` wraps around every operation on a backend
(`src/storage.rs:254`). Three properties make its behavior non-obvious; all are
confirmed in the OpenDAL 0.55 source
(`opendal-0.55.0/src/layers/concurrent_limit.rs`):

1. **One global semaphore per operator.** It is shared across *everything* that
   backend does — all `--concurrency` checkpoints and all their fanned-out file
   ops contend for the same N permits.

2. **The permit is held for the entire transfer, not just the request.** For
   `read` and `write`, the layer acquires an *owned* permit and attaches it to
   the returned `Reader`/`Writer`; it is released only when that reader/writer
   is dropped (the source comments: *"Hold on this permit until this reader has
   been dropped"*, line 161). Consequences:
   - For **verify / mirror**, `--max-concurrent 64` means *at most 64 files
     being actively streamed/downloaded at any instant*.
   - For a plain **existence scan**, the op is `stat`, which uses a *non-owned*
     permit released the moment the metadata call returns — so 64 means 64
     in-flight metadata round-trips (light per-op, but a high request rate).

3. **Source and destination get separate operators, hence separate
   semaphores.** A mirror can have up to 64 reads from the source *and* 64
   writes to the destination simultaneously.

The HTTP client (`src/storage.rs:402`) sets **no per-host connection limit**
(reqwest's default `pool_max_idle_per_host` is unbounded, and there is no
max-total-connections setting). So for HTTP backends this semaphore is *the*
effective ceiling on simultaneous TCP connections / in-flight requests.

> Nuance: if the server speaks HTTP/2, those 64 ops multiplex over far fewer TCP
> connections as parallel streams; over HTTP/1.1 it is ~64 distinct connections.
> S3 is typically HTTP/1.1; CDN-fronted hosts are often HTTP/2.

The layer also exposes an optional, separate `http_semaphore`
(`with_http_concurrent_limit`). This codebase does **not** use it, so there is
only the one operation-level limit.

## Why 64 is the binding throttle

Per checkpoint, `process_checkpoint` (`src/pipeline.rs:378`) fans out
concurrently:

- 3 required files — ledger / transactions / results (`join_all`)
- + 1 optional scp file
- + 1 history file, then `process_buckets` (`src/pipeline.rs:483`) `join_all`s
  over every *new* (non-deduplicated) bucket referenced by that checkpoint

That is roughly **~5–10 concurrent ops per checkpoint**, spiking higher near
genesis where many buckets are fresh (bucket dedup across checkpoints is handled
by an LRU cache, so steady-state checkpoints add only a few new buckets).

With `--concurrency 32`, the pipeline *wants* roughly `32 × 5–10 = 160–320` ops
in flight, but `--max-concurrent 64` caps it. Two consequences follow:

- **Raising `--concurrency` without raising `--max-concurrent` buys no extra
  backend parallelism.** It just queues more checkpoint state behind the
  semaphore and raises memory.
- **Raising `--max-concurrent` above what the checkpoint fan-out can generate
  does nothing** unless `--concurrency` is also high.

The two knobs must move together to have an effect.

## Implications of increasing `--max-concurrent`

**Upside (only if the backend isn't already saturated):** more overlapping
requests hide round-trip latency. This matters most when ops are *latency*-bound
(small files / high-RTT remote / metadata scans), and less when they are
*bandwidth*-bound (a few large streams already fill the pipe).

**Costs, by resource:**

- **Server health.** N simultaneous connections/requests to one host. On a
  shared public archive this invites 429/503 rate-limiting, throttling, or a
  soft ban, and degrades service for other users. The tool's automatic retries
  then become a retry storm that *adds* load.
- **Local sockets / file descriptors.** Each held read is an open socket + TLS
  session; high N can hit `ulimit -n` or exhaust ephemeral ports (HTTP/1.1
  only).
- **Memory.** Every concurrent stream carries buffers (and, for verify, a
  decompress task plus a 64-deep × 64 KB channel each). RSS grows roughly with
  the number of *active* transfers, which this knob governs directly.
- **CPU.** Verify is gzip + SHA256; mirror-verify likewise. Past roughly the
  physical-core count of concurrent hashers you get scheduling overhead, not
  throughput.

Past the real bottleneck, more concurrency only adds queueing latency, memory,
and instability. (See `docs/perf-report.md`: local throughput plateaus at
`c≈2–4` on a 10-core box, and mirror is flat across all `-c` because it is
disk-bound.)

## How to pick the optimal number

### 1. Find the dominant bottleneck — it differs by mode and backend

| Scenario | Bound by | Optimal `--max-concurrent` |
|----------|----------|----------------------------|
| Remote, existence scan (`stat`) | RTT / request rate | High helps (hides latency) — until the server pushes back |
| Remote, mirror/verify (large reads) | network bandwidth | `≈ bandwidth ÷ per-stream throughput` — usually a *handful* saturates a fat pipe; more just splits it |
| Local verify | CPU (gzip + sha256) | `≈ physical cores`; above that, nothing |
| Local mirror | disk write | low; more thrashes |

### 2. Use Little's Law as the mental model

Concurrency needed to saturate a resource ≈ `target throughput × latency per
op`. To keep bandwidth `B` full with per-stream rate `r`, you need ~`B/r`
streams; to hide RTT across many small files, ~`RTT ÷ time-per-file`. Once
you've covered the bandwidth-delay product, additional permits do nothing
useful.

### 3. Sweep it empirically — don't guess

Hold `--concurrency` high enough that it isn't the limiter, vary
`--max-concurrent`, and watch three signals:

- **Throughput (MB/s or files/s)** → stop at the *knee* where it plateaus.
- **`retry_count` / per-type failures in the stats report** → the backend-health
  feedback signal. If retries (429/503) climb as you raise the knob, you're
  stressing the server — back off below that point. (The perf harness already
  does this kind of sweep for `-c`; it has never varied `--max-concurrent`,
  which is the open gap.)
- **Peak RSS and FD count** → local pressure ceiling.

The optimum is the knee, with margin kept *below* where errors start.

### 4. Set the guardrail by who owns the backend

- **Public shared archive (`history.stellar.org`):** 64 is a polite default. If
  scans are slow and `retry_count` stays at zero, try 128. Treat rising retries
  as a hard stop — be a good citizen.
- **Your own S3 / GCS bucket:** the limiter is your wallet and local resources,
  not backend health. Object stores absorb thousands of req/s (S3 ≈ 5,500
  GET/prefix/s), so push to 128–512 for throughput and let RSS/CPU be the
  ceiling.
- **`file://`:** the backend isn't the constraint — CPU (verify) or disk
  (mirror) is. Going above ~core-count does nothing; tune `--concurrency` down
  instead.

### 5. Complementary knob: `--bandwidth-limit`

If the goal is to be polite about *total bytes* rather than *connection count*,
use `--bandwidth-limit` (the OpenDAL `ThrottleLayer`, `src/storage.rs:266`)
alongside. It caps throughput regardless of how many streams are open, which is
sometimes a better fit for "don't hammer the server" than reducing concurrency.

## Bottom line

`--max-concurrent` is a global, transfer-duration-held semaphore that is the
true connection cap for remote backends. The right value is the knee of a
throughput-vs-concurrency curve for your specific mode + backend, chosen to sit
just below where the backend starts returning retryable errors. For the public
archive, 64 is a sensible polite default; for infrastructure you own, it is
deliberately conservative and can be raised substantially — backed by a sweep.
