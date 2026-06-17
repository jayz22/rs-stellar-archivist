# `--max-concurrent` experiment — working progress report

> **Status:** IN PROGRESS. Phase A (existence-scan) complete + thoroughly
> instrumented. Phase B (scan-verify) not yet run. Branch `perf-verify-speedup`,
> worktree `/home/jay/Projects/rs-stellar-archivist-verifyperf`.
> Plan: `docs/max-concurrent-sweep-plan.md`. Parked alternative:
> `docs/max-concurrent-local-http-sim.md`. Last updated 2026-06-17.
>
> Methodology deep-dives in §2.1–2.3 (nested concurrency / cores-vs-concurrency /
> semaphore starvation) and Appendices A–C (Linux threads & tokio runtime;
> `futex`/`epoll`; connection & RTT measurement + the RTT-mean artifact). The RTT
> metric was corrected to median-SRTT + minRTT (`nethealth.sh [D]`); see §7 + App C.

## 1. What we're answering

Find the `--max-concurrent` value that maximizes throughput against a **remote**
archive without harming backend health — and back the recommendation with
concrete evidence (core utilization, thread state, connection count, TCP health),
not just wall-clock numbers.

- **Target (this phase):** public `history.stellar.org/prd/core-live/core_live_001`
  over HTTPS. Box `user-dev-007` (AWS Graviton2, aarch64, 32 vCPU, ~123 GiB),
  quiet (load ~0.04, no competing process). Path RTT to the CDN edge ~1–2 ms warm
  (`minRTT`), ~7 ms cold-connect (Appendix C).
- **Scope decided:** existence-scan + scan-verify. **This report = existence-scan.**
- Binary: `bin/sa-perf` (`--features perf-metrics`, miniz backend — irrelevant
  here, the work is network-bound not gzip-bound).
- 1 rep per cell (per agreed protocol); confidence comes from the large range.

## 2. The two knobs (grounded in source)

- **`-c` / `--concurrency`** (default 32): `pipeline.rs:327` —
  `stream::iter(cps).for_each_concurrent(self.config.concurrency, ...)`. It bounds
  how many **checkpoint futures** are in flight. These are async tasks on tokio's
  default multi-thread runtime (`#[tokio::main]`, `main.rs:5` → worker threads =
  nproc = 32). **`-c` is NOT threads or cores.**
- **`--max-concurrent`** (default 64): `storage.rs:254` —
  `opendal::layers::ConcurrentLimitLayer::new(config.max_concurrent)`, a semaphore
  applied to each OpenDAL `Operator` capping simultaneous I/O **operations** per
  backend. The HTTP backend is `opendal::services::Http` over a reqwest client
  (`storage.rs:401`), so each permit manifests as one in-flight HTTP request ≈ one
  live TCP connection — the effective **connection cap**. Inert for `file://`.

### 2.1 How the two knobs interact (nested concurrency)

They are two **nested** levels of concurrency, not two independent dials:

```
 for_each_concurrent(-c)        ← up to  -c  checkpoint futures alive at once
   │  each checkpoint future issues a BURST of file requests concurrently:
   │  join_all(ledger, transactions, results) + scp + (HAS → bucket fetches)
   │  (process_checkpoint, pipeline.rs:378)            ── this is the "fan-out"
   ▼
 ┌──────────── all requests funnel through one gate ────────────┐
 │   ConcurrentLimitLayer semaphore, width = --max-concurrent    │
 └───────────────────────────────────────────────────────────────┘
   ▼
 requests actually on the wire  ≈  open TCP connections  ≈  --max-concurrent
```

- **`-c`** = how many checkpoint workers are *trying* to push requests into the pipe.
- **`--max-concurrent`** = the *width of the pipe* — at most this many requests are
  in flight at once; the rest `await` a permit.
- Requests in flight at any instant = **`min(demand, --max-concurrent)`**, where
  **`demand ≈ (active checkpoints) × (concurrent requests each is issuing)`**.

### 2.2 Why core count barely matters here (concurrency ≠ parallelism)

- **Cores give parallelism** — simultaneous *CPU execution* (at most 32 things
  computing at once).
- **The knobs give concurrency** — simultaneous *outstanding operations*, which for
  network I/O are almost entirely **waiting**, not computing.

When a checkpoint future sends a request and `await`s, tokio **parks** it and frees
the core for another future. An in-flight request costs ~no CPU (a parked future +
a kernel socket). So 32 cores can babysit hundreds of in-flight requests — measured:
at `--max-concurrent=256`, **280 live connections but <2 of 32 cores used**, 30
cores idle. Neither knob is bounded by core count for I/O-bound work. (It *would*
be for CPU-bound work — e.g. the local verify long-pole — where useful concurrency
caps near the core count because the futures actually need cores to run.)

### 2.3 Knob protocol (isolation) — why hold `-c` high, and "starving the semaphore"

To find `--max-concurrent`'s ceiling, the semaphore must be the **only** limiter, so
that setting `--max-concurrent=N` actually achieves N-way concurrency. Effective
concurrency = `min(demand, mc)`. If `demand` (set by `-c`) is below `mc`, then **`-c`
is secretly the limiter** — raising `mc` does nothing and the curve falsely flattens,
which would read as "mc above X doesn't help" when really `-c` capped it.

**"Starving the semaphore"** is that failure mode: too few pending requests to fill
the permits, so permits sit **idle** and effective concurrency falls short of `mc`.
Example: if each checkpoint keeps ~4 requests in flight and `-c=4`, demand ≈ 16; set
`--max-concurrent=128` and only ~16 of 128 permits are ever taken — you *think* you
tested 128-way, but you tested 16-way. That data point is meaningless.

**Therefore:** hold `-c` fixed and high enough that fan-out always supplies
`demand ≥ mc` across the whole sweep, then sweep `--max-concurrent` as the sole
variable. We used `-c=256` (large surplus); the §6 control confirms the surplus was
harmless and `-c=32` already saturates the grid.

### 2.4 Mental model: when do the cores actually get used?

The three components map to distinct resources:

| Component | What it is | Bounded by | Costs |
|---|---|---|---|
| **Threads (~37)** | 32 tokio **worker** threads (= `nproc`, run futures incl. XDR-parse / gzip / SHA) + 1 main + ~4 lazy **blocking** threads (DNS/blocking syscalls) | **Hardware** (the 32; the rest is runtime overhead, not core-tied) | CPU |
| **`-c`** | checkpoints that may simultaneously produce I/O requests | the value you set | RSS (in-flight checkpoint state) |
| **`--max-concurrent`** | OpenDAL semaphore on in-flight requests ≈ live TCP connections | the value you set | **local I/O**: FDs, ephemeral ports, socket buffers, epoll/syscall overhead (this is why mc=512 regressed) — *not* CPU |

**The causal chain:** a served response delivers bytes → its future becomes
runnable → a **worker thread** does the CPU work (decompress + SHA + XDR-parse for
verify; ~nothing for existence). Cores stay idle — and adding cores won't help —
whenever not enough CPU-generating work is in flight, because of:

1. **low `-c`** → too few checkpoints generating requests (supply-starved), or
2. **low `--max-concurrent`** → semaphore caps in-flight requests (gate-starved), or
3. **trivial work per response** → existence-scan just checks an HTTP status; ~zero
   CPU. **← this experiment's case** (network-latency-bound, <2 of 32 cores), or
4. **a different limiter binds first**, even with high `-c`, high `mc`, *and*
   CPU-heavy responses: **network bandwidth** (link maxes out before the cores do)
   or a **single-stream long-pole** (the 2.36 GB bucket is one serial gzip stream —
   one core busy, 31 idle).

So "served responses generate CPU work" is **necessary but not sufficient** for core
saturation: the work must also arrive faster than 32 cores can drain it *and* not be
capped first by bandwidth or a serial long-pole. Cases 1–3 explain why
**existence-scan** is latency-bound and core-idle; case 4 is the open hypothesis for
**Phase B (verify)** — there *is* real CPU work, but we expect bandwidth or the
long-pole to bind before the cores do, which is why we measure it.

## 3. Instruments

- `scripts/perf/maxconc_sweep.sh` — the sweep driver (mirrors `verify_subsweep.sh`):
  fixed `-c`, ascending `--max-concurrent` ramp, 1 rep, per-cell `report.json` +
  `headline.csv`, mid-run sampling, retry/fail **hard stop**.
- `scripts/perf/nethealth.sh` — per-window evidence sampler (no root):
  - `[A]` cores the app actually uses (of 32) + system busy-core count
  - `[B]` thread count, running vs parked, and **what parked threads wait on** (wchan)
  - `[C]` established/`syn-sent` connections to `:443` (this pid)
  - `[D]` per-connection RTT from `ss -i` (kernel `tcp_info`): **median SRTT** +
    **minRTT** (the concurrency-independent path floor) + per-socket retransmits.
    Median/minRTT chosen over the mean because the mean is skewed by per-connection
    retransmit/delayed-ACK outliers — see Appendix C.
  - `[E]` **archive health:** TCP retrans %, timeouts, lost-retrans, syn-retrans,
    attempt-fails, estab-resets over the window (`:443` system-wide — attributable
    on this quiet box)
- `scripts/perf/bottleneck.sh` — named limiter verdict (CPU/disk/net/concurrency).

## 4. Results — existence-scan `--max-concurrent` sweep

**Large range: 10,000 checkpoints = 130,882 file existence checks per cell**,
`-c=256` fixed, range `--low 62406015 --high 63046015`. **0 failed, 0 retries on
every cell.**

| `--max-concurrent` | wall | files/s | speedup | max conns | app cores (of 32) | threads (run/tot) |
|---:|---:|---:|---:|---:|---:|---:|
| 8 | 463.9 s | 282 | 1.0× | 10 | 0.04 | 0/37 |
| 16 | 77.7 s | 1,685 | 5.97× | 29 | 0.27 | 0/37 |
| 32 | 36.6 s | 3,579 | 12.7× | 49 | 0.56 | 0/37 |
| 64 *(default)* | 17.7 s | 7,395 | 26.2× | 69 | 1.07 | 0/37 |
| **128** | **10.4 s** | **12,607** | **44.7×** | 144 | 1.76 | 0/37 |
| 256 | 9.6 s | 13,585 | 48.2× | 280 | 1.82 | 0/37 |

(Confirmation run at 2,000 cp / `-c=512`, grid to 512, agreed: same shape; **mc=512
regresses** to 9,341 files/s vs 10,802 at 256 — connection-management overhead.)

**Knee at `--max-concurrent` ≈ 128**, statistically flat 128↔256 (+8%), regression
by 512. **The default of 64 leaves ~1.7× on the table** for existence-scan.

### Curve files
`perf-results/maxconc/exist_big/curve.csv`, `perf-results/maxconc/exist/curve.csv`.

## 5. Evidence — where is the bottleneck? (cores/threads)

The app uses **<2 of 32 cores at every concurrency level**; all 37 threads
(32 tokio workers + ~5) are **parked** every sample (0 running), waiting on
`futex_wait_queue` / `ep_poll`. `bottleneck.sh` verdict is **NETWORK-LATENCY /
CONCURRENCY-bound at all six cells** — CPU idle, disk idle, RX <1 MB/s.

Existence-scan is pure round-trip waiting — there is **no CPU work to parallelize
across cores**. `--max-concurrent` helps by keeping more requests *in flight*
(Little's law: throughput = concurrency ÷ latency), confirmed by **established
connections tracking mc 1:1** (8→10, 64→69, 128→144, 256→280). This is the remote
mirror image of the local verify finding (there: CPU long-pole; here: RTT).

## 6. Evidence — `-c` is futures, not cores (control experiment)

Fixed `--max-concurrent=64`, 6,000 cp, vary `-c`:

| `-c` | wall | threads | app cores | RSS |
|---:|---:|---:|---:|---:|
| 32 | 11,141 ms | 36 | 1.07 | 64 MB |
| 64 | 11,046 ms | 37 | 1.03 | 68 MB |
| 512 | 10,989 ms | 37 | 1.13 | **123 MB** |

**Throughput identical** across `-c`. `-c=512` produces **37 threads, not 512** —
it does **not** oversubscribe the 32 cores (CPU flat ~1.1). The only cost of high
`-c` is **RSS** (more in-flight checkpoint state). So `-c=32` yields the same
result as `-c=512`, provided `-c` is high enough to feed the semaphore. Files:
`perf-results/maxconc/c_control/curve.csv`.

## 7. Evidence — archive health per concurrency level

| `--max-concurrent` | conns | retrans % | timeouts | attempt-fails | app retries | app failed |
|---:|---:|---:|---:|---:|---:|---:|
| 8 | 10 | 0.000% | 0 | 0 | 0 | 0 |
| 16 | 29 | 0.000% | 0 | 0 | 0 | 0 |
| 32 | 49 | 0.000% | 0 | 0 | 0 | 0 |
| 64 | 69 | 0.000% | 0 | 0 | 0 | 0 |
| 128 | 144 | 0.000% | 0 | 0 | 0 | 0 |
| 256 | 280 | 0.000% | 0 | 0 | 0 | 0 |

**Zero retransmits, timeouts, attempt-fails, and app-level retries/failures at
every level — including 280 concurrent connections.** No stress on the public
archive in the tested range (≤256). Raw per-window data in each cell's
`nethealth.txt`.

**On RTT (corrected):** the per-connection **path RTT (`minRTT`) is ~1–2 ms** to the
Cloudflare edge and is **independent of concurrency** — the network does not slow
down as we open more connections. An earlier draft reported a *mean* SRTT that
appeared to *fall* with concurrency (≈28 ms at mc=8 → ≈6 ms at mc=256); that was a
**measurement artifact**, not a network effect: the mean of smoothed-RTT is
dominated by a few per-connection outliers (a connection that retransmitted, or
landed on a slower edge IP), and those outliers make up a large fraction of a small
connection pool at low `mc` but wash out among hundreds at high `mc`. The instrument
now reports **median SRTT + minRTT**, which do not show the spurious trend. Full
dissection with the per-socket distribution: **Appendix C**. (A genuine RTT rise
with concurrency *is* possible under bandwidth saturation — not reached by
existence-scan; a signal to watch in the Phase B verify sweep.)

## 8. Conclusions (existence-scan)

1. **Optimal `--max-concurrent` ≈ 128** for existence-scan against the public
   archive; 256 is marginally better but doubles connections for +8%; **512
   regresses**.
2. The current **default of 64 is ~1.7× too low** for this latency-bound workload.
3. The knob is purely about **hiding network RTT**, not CPU — the box stays <2
   cores busy; raising `--max-concurrent` raises in-flight requests/connections.
4. `-c` is independent: hold it modestly high (≥ enough to feed the semaphore);
   it costs only RSS, not CPU/cores. The default `-c=32` is already sufficient.
5. The public archive showed **zero health degradation through 256 connections**,
   so the polite-default recommendation can be ~128 with comfortable margin.

## 9. Next steps

- **Phase B — scan-verify sweep** (bandwidth-bound; downloads + decompresses
  buckets). Expect the knob to help far less: near the chain top, dedup leaves a
  fixed distinct-bucket set dominated by one 2.36 GB single-stream bucket (a serial
  long-pole, per the resolved verify experiment). Open decision: range size & grid
  ceiling (download cost ~3–4 GB/cell). Same instrumentation (`nethealth.sh` will
  add net-BW saturation as the relevant signal).
- Then synthesize a recommended default for the public archive (polite) — and note
  that the *true* high-concurrency ceiling is better found with the parked
  local-HTTP-sim (`docs/max-concurrent-local-http-sim.md`), which removes politeness
  limits and can sweep RTT itself.
- **Single-knob design (future):** `docs/single-knob-concurrency-plan.md` — derive
  `--max-concurrent` from `-c` (or vice-versa) and expose one knob. Includes the
  (c, mc) 2D-surface test plan to settle whether optimal-`mc` is backend-constant or
  `c`-scaled, and to score candidate mappings (`mc=4c` vs constant-`mc`+min-`c`).

---

# Appendices — Linux kernel & measurement terminology

> Supplemental background for the evidence in §5–§7: what the numbers are, where
> they come from, and how to read them. Nothing here changes the conclusions; it
> makes them auditable.

## Appendix A — Threads, tasks, and the tokio runtime

- **Process vs thread (Linux).** A process is a group of **threads**; the kernel
  schedules threads, and exposes each as a "task" directory under
  `/proc/<pid>/task/<tid>/`. We count threads with `ls /proc/<pid>/task | wc -l`
  and read each thread's name from `…/comm`.
- **What the ~37 threads are.** Live capture at `mc=256` showed `36 tokio-runtime-w`
  + `1 sa-perf`:
  - `sa-perf` ×1 — the main thread (ran `fn main`).
  - `tokio-runtime-w` ×36 — tokio runtime threads (name truncated from
    `tokio-runtime-worker` at Linux's 15-char `comm` limit). Two roles share the
    name: the **worker pool = 32 threads** (tokio's default = number of logical CPUs
    = `nproc`; these poll/run futures), plus **~4 blocking-pool threads** spawned
    *lazily* for operations that can't be async — chiefly **DNS** (`getaddrinfo` is
    a blocking syscall) and some filesystem metadata; they retire after idle.
  - 32 + ~4 + 1 ≈ 37, and the count drifts 34–37 as blocking threads come and go.
- **Why thread count is tied to cores, not the knobs.** The worker pool is sized to
  `nproc`; `-c` and `--max-concurrent` add *futures and sockets*, not threads. This
  is why the §6 `-c` control showed 36–37 threads whether `-c=32` or `-c=512`.
- **Thread state** (field 3 of `…/stat`): `R` = runnable/on-CPU, `S` = interruptible
  sleep (scheduled off, consuming **zero** CPU). Across samples we saw ~1 `R` and the
  rest `S` — i.e. the cores were idle by design.

## Appendix B — `futex` and `epoll`: what the parked threads wait on

A sleeping thread's **wait-channel** (the kernel function it is blocked in) is read
from `/proc/<pid>/task/<tid>/wchan`. The two we observed:

- **`ep_poll` (epoll)** — Linux's **I/O event-notification** facility. A thread
  registers many file descriptors (here, hundreds of sockets) with an epoll instance
  and calls `epoll_wait()`, sleeping until *any* of them becomes ready (bytes
  arrived, socket writable…). This is the core of async I/O: **one** thread watches
  **all** sockets and wakes only on real activity, instead of one blocked thread per
  socket. A thread in `ep_poll` = "asleep until the archive sends bytes on one of
  these connections" — the network-latency-bound fingerprint.
- **`futex_wait_queue` (futex = fast userspace mutex)** — the kernel's generic
  **thread-parking / synchronization** primitive. A thread waiting for a condition
  another thread will signal (a lock, a queue, "is there a runnable task for me?")
  parks on a futex. In tokio, an **idle worker with no ready future parks on a
  futex**. So "34 threads on `futex_wait_queue`" = "34 workers idle, asleep, nothing
  to compute."

**Combined picture (the I/O-bound signature):** ~1 thread running, 1–2 in `ep_poll`
(the I/O reactor watching all sockets), the rest parked on `futex` — 30+ cores idle
while hundreds of requests sit outstanding in the kernel. More cores would not help;
more in-flight requests (`--max-concurrent`) would.

## Appendix C — How connections and RTT are measured (and the RTT-mean artifact)

### Counting connections
`ss -tnp` (socket statistics: **t**cp, **n**umeric, with **p**rocess info). We count
ESTABLISHED sockets owned by the pid: `ss -tnpH | grep -c "pid=<pid>,"`. Connections
track `--max-concurrent` because **HTTP/1.1 cannot multiplex** — one connection
carries one request at a time, so N concurrent requests need N open connections. The
small surplus above `mc` (e.g. 278–287 vs 256) is hyper's **keep-alive pool**: warm
idle connections kept for reuse, caught by the point-in-time `ss` snapshot.

### Measuring RTT
RTT comes from `ss -i` (`--info`), which dumps the kernel's `struct tcp_info` for
each socket — **not** from pings. Relevant fields (raw example
`rtt:7.23/0.861 … rto:208 … minrtt:1.141`):
- **`rtt:SRTT/rttvar`** (ms) — the **smoothed RTT** and its variance. The kernel
  measures each segment against its ACK (an RTT *sample*) and folds samples into an
  EWMA (Jacobson/Karels): `SRTT ← (1−α)·SRTT + α·sample`. This is the value the stack
  acts on (it derives `rto`, the retransmit timeout, from it).
- **`minrtt`** — the lowest RTT ever seen on the connection: the **uncongested path
  floor**.
- Parsing caveat: the substring `rtt:` also occurs inside `minrtt:` and `rcv_rtt:`,
  so the parser must guard with a non-letter/underscore lookbehind to read the
  standalone SRTT field. (The earlier draft did not, conflating the three.)

### What RTT here is and isn't
It is **TCP-path RTT to the CDN edge** — the *floor* of request latency. It does
**not** include TLS, origin/server processing, or transfer time, so end-to-end HTTP
service time is higher. The filter is system-wide `:443`, attributable only because
the box is quiet.

### The "RTT grew at low concurrency" artifact (why we switched to median + minRTT)
Per-connection path RTT is **concurrency-independent** — `minRTT` is ~1–2 ms whether
8 or 280 connections are open. The earlier *mean* SRTT appeared to fall with
concurrency purely as a **statistics artifact**. Live per-socket distribution at
`mc=8` (sorted by SRTT desc), `ss -ti` showing SRTT, minRTT, idle time, retransmits:

```
 srtt_ms  minrtt_ms  idle_ms  retrans
   64.60     62.52      324       5    ← one connection retransmitted 5× (packet loss)
   31.72     26.54     5318       0    ← a slower/farther CDN edge IP
    9.07 …  6.29       ~1–2  ──         the healthy majority (SRTT ≈ 6–9 ms)
    1.7  …  1.18       ~1–2  (idle)     idle keep-alive: SRTT near the true floor
   N=37   mean(SRTT)=6.0   max=64.6  |  most minRTT ≈ 1–2 ms
```

Two compounding causes:
1. **Per-connection SRTT inflation unrelated to concurrency:** a **retransmission**
   (Karn's algorithm keeps the smoothed value high after a loss), a slower **edge
   IP**, or **delayed-ACK** (a connection that sends a small final segment then idles
   gets its last ACK delayed ~40 ms — the `ato:40` timer — bumping SRTT).
2. **Outlier-sensitive averaging over a population that scales with `mc`:** at low
   `mc` one 64 ms connection is a large fraction of a tiny pool and drags the mean
   up; at high `mc` the same one or two outliers are diluted among hundreds of ~5 ms
   connections. Hence mean-SRTT *looked* like it improved with concurrency.

**Fix:** the instrument (`nethealth.sh [D]`) now reports **median SRTT** (robust to
outliers) and **minRTT** (the true path floor). Neither shows the spurious trend.
The `476 ms` max seen in the earlier table was almost certainly a single connection
mid-retransmission, not a systemic stall.

**Caveat for Phase B:** under **bandwidth saturation** (many connections filling the
link → queueing / bufferbloat), RTT *can* legitimately rise with concurrency. The
verify sweep moves GB and may approach the link ceiling, so a real RTT rise there
would be a genuine saturation signal — read median SRTT *and* minRTT together to tell
true queueing apart from outlier noise.
