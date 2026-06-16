# Finding the Optimal `--max-concurrent` — Experiment Plan

> **Status:** DRAFT — awaiting approval. Companion to `docs/max-concurrent-tuning.md`
> (the conceptual guide) and `scripts/perf/bottleneck.sh` (the instrument).
> Lives on the `perf-verify-speedup` worktree but is **orthogonal** to the verify
> CPU-speedup experiment: that one is local-CPU (gzip/SHA on `file://`); this one
> is **remote-I/O concurrency** (the connection cap to a network archive).

---

## 1. Goal & definition of "optimal"

Find the `--max-concurrent` value that **maximizes throughput** for each
(mode × backend) **without** pushing the backend into retryable errors or
blowing a local resource ceiling. Concretely, the optimum is:

> the **knee** of the throughput-vs-`--max-concurrent` curve, taken with margin
> **below** the point where `retry_count` / per-type failures start to climb,
> and below the local FD / RSS ceiling.

Deliverable values: a recommended default for the **public archive** (polite),
and a separate, higher recommendation for **infrastructure we own** — exactly
the split the tuning guide calls for, but currently unbacked by data.

## 2. Why this is a *remote* experiment (and `file://` is excluded)

`--max-concurrent` is a per-backend semaphore on **simultaneous I/O ops**, the
permit held for the **whole transfer** (tuning guide §"What it sets"). It is the
effective TCP-connection cap for HTTP backends. For `file://` the backend is
never the constraint — CPU (verify) or disk (mirror) is — so sweeping it there is
a no-op by construction. **Every cell here runs against the remote pubnet
archive over HTTPS.** This also means the experiment belongs with Stage 2.2
(full pubnet), not the §6.1 local fixture.

This connects to the verify-speedup finding: locally the verify engine uses
<1 core because it's latency-bound on the in-process channel. **Remotely**, the
dominant latency is the network RTT per request, which `--max-concurrent` hides
by overlapping more in-flight transfers — so the knob can matter a lot remote
even though it's inert local.

## 3. The knob protocol (how to isolate `--max-concurrent`)

The two knobs interact (tuning guide §"Why 64 is the binding throttle"): raising
`-c` without `--max-concurrent` only queues checkpoint state; raising
`--max-concurrent` above what the per-checkpoint fan-out (~5–10 ops/cp) can
generate does nothing. To isolate `--max-concurrent` as the sole variable:

- **Hold `-c` fixed and high** — high enough that checkpoint parallelism is never
  the limiter for any cell. With ~5–10 ops/cp, `-c = 128` wants 640–1280 ops in
  flight, enough to keep a semaphore of up to ~512 full. Use **`-c = 128`** for
  every cell (note: this inflates baseline RSS uniformly; the *delta* across
  cells still tracks `--max-concurrent`, which governs *active* transfers).
- **Vary `--max-concurrent`** across the grid below.
- Keep everything else at defaults (retries=3, timeouts, no `--bandwidth-limit`
  except the politeness leg in §7), no `--debug/--trace`.

## 4. Signals captured per cell (the three the guide names + limiter id)

| Signal | Source | Read as |
|---|---|---|
| **Throughput** (MB/s, files/s) | `run.sh` headline / `report.json` | stop at the *knee* where it plateaus |
| **`retry_count` + per-type failures** | `report.json` `summary.retries`, `files/buckets/checkpoints` | **backend-health hard stop** — rising retries = stressing the server |
| **Peak RSS + FD/conn count** | `/usr/bin/time -v`; `bottleneck.sh` [4] conns; `ls /proc/PID/fd` | local pressure ceiling |
| **Named limiter** | `bottleneck.sh` mid-run (CPU / net-BW / net-reqs / conns / disk / lock) | *why* it plateaued — tells us if raising further could help |

`bottleneck.sh` is the key addition over the `-c` sweep: at each cell it says
whether we're network-bandwidth-bound (RX near the ~203 MB/s single-stream
ceiling → stop), network-latency/concurrency-bound (CPU+disk idle, BW < ceiling
→ raising helps), or backend-pushing-back (retries/conns).

## 5. Modes / dimensions to sweep (each a different bottleneck)

Per the guide's scenario table, the optimum differs by mode:

1. **Remote existence scan (`scan`, no `--verify`)** — RTT / request-rate bound,
   tiny payload. The cleanest, cheapest, lowest-load sweep; `--max-concurrent`
   should help the most here (hides RTT across many small `stat`s). Wide grid.
2. **Remote `scan --verify`** — bandwidth + decompress. Expect a low knee
   (~`bandwidth ÷ per-stream rate`); a handful of streams saturate the pipe.
3. **Remote `mirror` (→ local `file://` dst)** *(optional)* — download-bandwidth
   bound; src uses `--max-concurrent`, dst is a separate (local) semaphore so it
   isn't the variable. Adds the write path; include only if (1)+(2) leave the
   mirror question open.

## 6. The grid + Little's-Law range estimate

- `--max-concurrent ∈ {8, 16, 32, 64, 128, 256}` (existence scan may extend to
  512). `-c = 128` fixed. **1 rep** per cell (signal-finding, like the
  verify-speedup sweep), median only if a cell looks noisy.
- **Window:** a **bounded recent remote range**, *much smaller* than the §6.1
  55 GB fixture, to keep each verify/mirror cell to minutes and limit load on the
  public archive. Proposal: **~200 checkpoints** off the recent tip
  (≈ a few GB). Existence-scan can use a larger range (no payload) — e.g. the
  full §6.1 window — since it only `stat`s.
- **Little's Law sanity:** measured single-stream pubnet BW ≈ **203 MB/s**. If
  per-stream verified throughput is ~`r` MB/s, ~`203/r` streams saturate the
  pipe — so the verify/mirror knee is expected at a *small* `--max-concurrent`,
  while the existence-scan knee (latency-bound) is much higher. The sweep
  confirms both and we stop once `bottleneck.sh` reports bandwidth-bound or
  retries appear.

## 7. Politeness / safety guardrails (public shared archive)

`history.stellar.org` is a **public, shared** archive. Hammering it is both
antisocial and self-defeating (retry storms). Non-negotiable rules:

- **Ramp upward, never start high.** Run cells low→high `--max-concurrent`.
- **Retries are a hard stop.** If `summary.retries` rises above a small
  threshold (e.g. >0 sustained / >0.5% of ops) at a cell, record it and **do not
  go higher** for that mode. The recommended value sits a notch *below* that.
- **Small windows, short runs.** Per §6, keep verify/mirror cells to a few GB.
- **One sweep at a time**, off-peak where possible. Optionally add a
  `--bandwidth-limit` politeness leg to show the byte-rate alternative to a
  connection cap.
- **Prefer an owned backend if the goal is to find the *ceiling*.** The guide is
  explicit: for buckets we own (S3/GCS), backend health isn't the constraint and
  we can push to 128–512 with RSS/CPU as the only limit. If we want the true
  high-concurrency knee, point the sweep at an **SDF-owned mirror/bucket**, not
  the public host. (Decision in §10.)

## 8. Phased execution (with approval gates, same as the verify experiment)

- **Phase A — existence-scan sweep** (cheapest, safest, most latency-sensitive).
  Establishes the method, the `bottleneck.sh` reading per cell, and the
  RTT-hiding knee. Present curve + limiter verdicts → approve.
- **Phase B — scan-verify sweep** on the small remote window. Find the
  bandwidth knee; confirm it's low (Little's Law). Present → approve.
- **Phase C — (optional) mirror sweep** + the `--bandwidth-limit` politeness
  leg. Present → approve.
- **Phase D — synthesis.** Throughput/retry/RSS curves per mode; recommended
  defaults (public vs owned); fold findings back into
  `docs/max-concurrent-tuning.md` and the perf report. Decide whether to change
  the shipped default of 64.

Each phase: results + conclusion presented, commit only after approval, raw
artifacts under `perf-results/maxconc/<mode>/` (gitignored).

## 9. Reuse / new harness

- Reuse `run.sh` (OS time + report.json) and `bottleneck.sh` (limiter id).
- One small new driver `scripts/perf/maxconc_sweep.sh`: for a mode, fix `-c`,
  loop `--max-concurrent`, run `run.sh`, and sample `bottleneck.sh` mid-cell into
  the run dir. (Mirrors `verify_subsweep.sh`.)
- Capture FD/conn count: `ss -tnp | grep pid=` and `ls /proc/PID/fd | wc -l` at
  the same mid-run instant as `bottleneck.sh`.

## 10. Decisions needed before running

1. **Backend target:** public `history.stellar.org` (polite ceiling, ~64–128) vs
   an **SDF-owned S3/GCS mirror** (find the true ceiling, 128–512)? Changes the
   grid ceiling and the politeness rules.
2. **Scope:** existence-scan only (cheapest, answers the latency-hiding question)
   vs existence + verify vs all three modes?
3. **Timing:** run now interleaved with the verify-speedup experiment, or after
   it wraps? (They don't contend — local-CPU vs remote-net — but sharing the box
   muddies `bottleneck.sh` CPU readings, so ideally not simultaneously.)
