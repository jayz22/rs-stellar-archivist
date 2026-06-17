# Finding the Optimal `--max-concurrent` — Experiment Plan

> **Status:** DRAFT — awaiting approval. Companion to `docs/max-concurrent-tuning.md`
> (the conceptual guide) and `scripts/perf/bottleneck.sh` (the instrument).
> Lives on the `perf-verify-speedup` worktree but is **orthogonal** to the verify
> CPU-speedup experiment: that one is local-CPU (gzip/SHA on `file://`); this one
> is **remote-I/O concurrency** (the connection cap to a network archive).

---

## 🟢 START HERE — current state & instructions for the next agent (2026-06-17)

**You (a fresh/compacted agent) are picking up the `--max-concurrent` experiment.**
Everything you need is in this doc + the two companions named above. Read this
section, then §3 (knob protocol), §4 (signals), §7 (politeness), §9 (harness).

### Where things stand
- **The verify CPU-scaling experiment is RESOLVED** (different effort, now done).
  One-line: the `-c≈4` verify plateau is an *intrinsic* multi-GB-bucket serial
  gzip long-pole; recommendation was *adopt `zlib-rs`, don't ship the sync
  rewrite*. Full write-up: `docs/perf-report-graviton-pubnet.md` §"Verify
  CPU-scaling experiment". **That work is not your concern** except that it built
  the harness/tooling you'll reuse.
- **This `--max-concurrent` experiment has NOT started.** It is the remaining open
  thread. `--max-concurrent` is inert for `file://` (local CPU/disk binds), so
  this is a **REMOTE** experiment — it needs a network archive.
- **The box is now quiet.** Host `user-dev-007` (AWS Graviton2, aarch64, 32 vCPU,
  ~123 GiB RAM; data volume `/data` = RAID0 NVMe). The Stage 2.2 full-pubnet
  mirror that was contaminating earlier measurements has **finished / is no longer
  running** (verified: no `*mirror*` process, load ~0). Measured pubnet link
  ≈ **203 MB/s single-stream** (see `perf-results/env.txt`). Re-confirm quiet with
  `scripts/perf/bottleneck.sh` before each run.
- Worktree: `/home/jay/Projects/rs-stellar-archivist-verifyperf`, branch
  `perf-verify-speedup` (pushed to `origin`). `cargo` is at `~/.cargo/bin`
  (non-interactive shells: `export PATH="$HOME/.cargo/bin:$PATH"`).

### ⛔ BLOCKER — get these 3 decisions from the user FIRST (see §10)
Do not start runs until the user confirms. My recommended defaults (propose these):
1. **Backend target → public `history.stellar.org`** (the realistic, polite
   target). Treat rising `retry_count` as a hard stop (§7). Only switch to an
   SDF-owned S3/GCS bucket if the user wants the *true high-concurrency ceiling*
   rather than the polite one.
2. **Scope → existence-scan + scan-verify** (existence-scan first: cheapest,
   safest, most latency-sensitive — the knob helps most there; then scan-verify
   for the bandwidth knee). Mirror optional/last.
3. **Timing → now** (box is quiet). Don't run while anything else loads the box;
   re-check with `bottleneck.sh`.

### Concrete next steps (once decisions confirmed)
1. Build a fresh `bin/sa-perf` (`--features perf-metrics`) on this box if not
   present (recipe in `perf-results/verifyperf/ARTIFACTS.md`). `zlib-rs` backend
   is fine to use but irrelevant here (remote = network-bound, not gzip-bound).
2. Write `scripts/perf/maxconc_sweep.sh` (see §9) — mirrors the existing
   `scripts/perf/verify_subsweep.sh`: fix `-c` HIGH (so checkpoint parallelism is
   never the limiter — see §3), loop `--max-concurrent ∈ {8,16,32,64,128,256}`
   (existence-scan may extend to 512), 1 rep, and sample `bottleneck.sh` mid-run.
3. Per cell capture the **three signals** (§4): throughput knee (MB/s, files/s);
   `retry_count`/per-type failures from `report.json` (**backend-health hard
   stop**); peak RSS + FD/conn count (`ss -tnp | grep pid=`, `ls /proc/PID/fd`).
   Plus `bottleneck.sh`'s named limiter verdict at the chosen `-c`.
4. **Politeness is mandatory** for the public archive (§7): ramp low→high, stop
   when retries climb, small windows, off-peak. Reuse the §6.1 fixture's
   `--low 62918015 --high 63046015` range for verify cells but FETCH REMOTE
   (`https://history.stellar.org/prd/core-live/core_live_001`), and bound smaller
   (~200 cp) for verify/mirror cells to limit load; existence-scan can use a
   bigger range (no payload).
5. Phase A→D with approval gates (§8): present results + conclusion after each,
   commit only after user approval, raw artifacts under
   `perf-results/maxconc/<mode>/` (gitignored; force-add the small text artifacts
   like the verify experiment did — see `perf-results/verifyperf/ARTIFACTS.md`
   policy: keep everything, one `SA_PERF_OUT` dir per run, never `SA_PERF_OUT=""`).

### Working-style reminders (from this project)
- This perf work uses `target/release` builds (never debug). Save every run's
  logs + `report.json` (the harness `run.sh` does this).
- Use `bottleneck.sh` (not `probe.sh`) for the limiter verdict — `probe.sh`'s
  per-process CPU read proved misleading in the verify experiment.
- Present findings + wait for approval before committing each phase; the user
  reviews per-phase.

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
