# Idea (PARKED): simulate a remote archive by serving the local mirror over HTTP

> **Status:** PARKED — captured for later. The active `--max-concurrent` sweep is
> running against the **public `history.stellar.org`** target instead (user
> decision, 2026-06-17). This doc preserves the local-HTTP-sim idea, the
> environment findings, the proposed plan, and the open questions so it can be
> picked up without re-discovery.
> Companion: `docs/max-concurrent-sweep-plan.md` (the active experiment plan).

## The idea

`--max-concurrent` is the per-backend I/O semaphore = effective TCP-connection
cap for HTTP backends. It is **inert for `file://`** (local CPU/disk binds, never
the backend). To exercise the knob *without* hammering the public archive and
*without* politeness constraints, **serve our existing full local archive copy
over HTTP on this box** and point archivist at `http://127.0.0.1:PORT/...`.

That turns a local data set into a real HTTP backend — the knob becomes active,
we control the "server," there are no politeness limits, and we can find the
*true* client-side concurrency ceiling (FD/conn scaling, semaphore overhead).

## Environment findings (2026-06-17, host `user-dev-007`, aarch64, 32 vCPU)

- **Archive to serve:** `/data/pubnet-mirror` is a complete, valid archive root.
  - `.well-known/stellar-history.json` present: `version 2`, stellar-core 27.0.0,
    `currentLedger 63051327`, pubnet passphrase. **Covers the verify range**
    (`--low 62918015 --high 63046015`, high < currentLedger).
  - Standard hex-prefixed layout: `bucket/ history/ ledger/ results/ transactions/`.
  - **Size: 7.2 TB, 10,597,825 files** (`du`/`find`, 2026-06-17). This is the full
    pubnet mirror, not the small verify fixture (`/data/perf/fixture`).
- **Box is quiet:** load ~0.04; no `mirror`/`archivist`/`sa-perf` process running
  (the Stage 2.2 mirror finished). Re-confirm with `scripts/perf/bottleneck.sh`.
- **No passwordless sudo** (`sudo -n` fails); `apt-get` exists but needs a password.
- **HTTP servers present:** only `python3` and `busybox`. **Absent:** nginx, caddy,
  miniserve, thttpd, darkhttpd, go.
  - ⚠️ Python `http.server` is **single-threaded** → it would itself become the
    bottleneck and we'd measure the *server*, not the client's `--max-concurrent`.
    `ThreadingHTTPServer` is GIL-limited and still weak under high concurrency.
- **Net tooling present:** `tc` (`/usr/sbin/tc`) and `ss` (`/usr/bin/ss`) → we can
  inject latency/bandwidth on `lo` and count live connections.
- **`cargo` present** (`~/.cargo/bin`, 1.96.0) → `cargo install miniserve` is a
  no-root way to get a genuinely multithreaded static server.

## Proposed plan (when resumed)

1. **Stand up a concurrent static HTTP server** rooted at `/data/pubnet-mirror`,
   bound to `127.0.0.1:PORT`. Server must NOT be the bottleneck (static gzip files
   + sendfile ≈ near-zero CPU). See open question #1 for the server choice.
2. **Model the "network."** Loopback RTT ≈ 25 µs, but the knob matters precisely
   because it hides per-request RTT (real pubnet link ≈ 203 MB/s with real RTT).
   On bare localhost the knee appears at trivially low concurrency and reflects
   server/CPU limits, not remote behavior. Use `tc netem` on `lo` to add a
   realistic RTT tier (~30 ms; optionally a ~200 MB/s bandwidth cap to match the
   measured real link). See open question #2.
3. **Point archivist at `http://127.0.0.1:PORT/`** and run the same sweep as the
   active plan: hold `-c` HIGH, sweep `--max-concurrent ∈ {8,16,32,64,128,256,512}`,
   1 rep, scope existence-scan + scan-verify, capture the three signals (throughput
   knee · `retry_count`/failures · RSS + FD/conn via `ss`/`/proc/PID/fd`) +
   `bottleneck.sh` verdict. Reuse `scripts/perf/{run.sh,bottleneck.sh}` and a new
   `maxconc_sweep.sh`.
4. Artifacts under `perf-results/maxconc-localhttp/<mode>/` (one `SA_PERF_OUT` dir
   per run, never `SA_PERF_OUT=""`, force-add small text artifacts).

## Open questions (unresolved when parked)

1. **Which concurrent HTTP server?**
   - `miniserve` via `cargo install` — Rust/actix, multithreaded, no root. ~1–2 min
     compile. **Recommended** (won't be the bottleneck, no password needed).
   - Threaded Python — available now, no install, but GIL-limited; risks capping
     concurrency and muddying the knee.
   - nginx — best static-serving (sendfile, near-zero CPU) but needs a password to
     `apt install` (run via the `!` prefix).
2. **Latency model?**
   - **Bare + netem RTT (recommended):** (a) bare loopback = pure client/server
     ceiling; (b) `tc netem` ~30 ms RTT so the knob's latency-hiding is real.
   - netem RTT only — skip the bare ceiling, only the realistic case.
   - Bare loopback only — fastest setup, but the knee won't represent a real
     remote archive (measures local plumbing, not RTT-hiding).

## Why this is valuable later

- A faithful, repeatable, politeness-free way to find the **true client-side
  ceiling** of `--max-concurrent` (where the client saturates / FD pressure / lock
  stalls appear) independent of a third party's rate limits.
- With `tc netem` we can *sweep RTT itself* — directly mapping how the optimal
  `--max-concurrent` shifts with link latency, which the public-archive run can't
  do (fixed real RTT, politeness-capped grid).
