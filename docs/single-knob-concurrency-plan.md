# Plan (FUTURE EXPLORATION): single-knob concurrency — deriving `--max-concurrent` from `-c`

> **Status:** DRAFT / not started. Future-exploration plan, captured 2026-06-17.
> Builds on the findings in `docs/max-concurrent-progress-report.md` (existence-scan
> `--max-concurrent` sweep + the `-c` control + the nested-concurrency model in
> §2.1–2.4). Depends on the parked `docs/max-concurrent-local-http-sim.md` for the
> verify part. Branch `perf-verify-speedup`.

## 1. Motivation

Today the user must set two interacting concurrency knobs and understand the
starvation rule between them:

- **`-c` / `--concurrency`** — checkpoints that may simultaneously generate I/O
  requests (`for_each_concurrent`, `pipeline.rs:327`).
- **`--max-concurrent`** — OpenDAL `ConcurrentLimitLayer` semaphore on in-flight
  I/O ops ≈ live TCP connections (`storage.rs:254`).

This is poor UX. **Goal: expose a single concurrency knob** and derive the other
internally, with sane bounds — so a user picks one number and gets near-optimal
throughput without reasoning about the interaction.

## 2. The two proposals on the table

### 2a. User's proposal — `mc = k·c` (linear coupling), e.g. k=4
- Keep `-c` as the single exposed knob.
- Derive `--max-concurrent = 4 × c`.
- **Cap `c` at 32**, i.e. `mc ≤ 128` connections.
- Premise: there is a stable "sweet spot" ratio between checkpoints generating
  requests and connections OpenDAL serves, and it shouldn't vary much.

### 2b. Counter-proposal — derive `c` from a backend-set `mc` (inverse coupling)
Rationale from the existing data:
- **Optimal `mc` is a property of the network/backend, not of `c`.** The existence
  knee sat at ~128 regardless of how requests were generated; it's set by RTT, link
  bandwidth, and the server's concurrency tolerance. A different backend moves it.
- **Optimal `c` is "just enough to feed `mc`, then as low as possible."** The `-c`
  control showed throughput flat from `c=32`→`c=512` at fixed `mc` — extra `c` buys
  nothing but **RSS**, and for verify/mirror `c` drives memory hard (more concurrent
  bucket decode buffers).
- Therefore a **fixed linear `mc=4c` ties a network constant to a memory-costing
  supply knob**: to reach mc≈128 you're *forced* to `c=32` (high RSS on verify),
  even though the network only needs `c` ≥ enough to feed the semaphore.
- The "sweet-spot ratio" is really the **minimum feed ratio** (`mc/c ≤ fan-out`) —
  a *floor* on `c`, not an *optimal* multiplier.

Inverse rule: pick `mc` from the backend (default ~128, eventually backend-tuned),
then `c = clamp(ceil(mc / fan_out_safety), c_min, c_max)` — the minimum `c` that
keeps the semaphore fed. Same single-knob simplicity, no forced memory.

**This plan does not pre-judge the winner — Step 3 scores both against measured data.**

## 3. Key empirical question

> Is optimal-`mc` a **backend constant** (independent of `c`), or does it **scale
> with `c`**? And is any single `mc:c` mapping near-optimal across modes
> (existence vs verify) and conditions (RTT, bandwidth)?

The answer decides which derivation direction is correct.

## 4. Test plan

### Step 1 — Measure effective fan-out `f`
At `c=1`, sweep `mc`. Throughput plateaus once `mc` exceeds what a single checkpoint
keeps in flight; `established conns ≈ f` at the plateau. Gives `f` separately for
**existence** and **verify** — the number the whole feed relationship hinges on.
(Fan-out is the burst from `process_checkpoint`, `pipeline.rs:378`: category files +
SCP + HAS + bucket refs, minus dedup-skipped buckets.)

### Step 2 — 2D throughput surface
Sweep `c ∈ {1,2,4,8,16,32}` × `mc ∈ {8,16,32,64,128,256}` (36 cells). Per cell
record: throughput (files/s or MB/s), peak RSS, max conns, `bottleneck.sh` verdict,
archive health (`nethealth.sh`: retrans/timeouts/fails, median SRTT + minRTT).
Produce a heatmap. Reveals:
- the **starvation frontier** (cells where `mc > c·f` → throughput tracks `c`),
- whether **optimal-`mc` is constant across `c`** or scales with it,
- the **RSS cost of `c`** at each point.

### Step 3 — Score candidate mappings against the measured optimum
For each `c`, compare throughput at: (a) `mc = 4c` (user), (b) `mc = const 128` with
min-`c` (counter), (c) `mc = 4c` capped at 128. Quantify the throughput gap **and**
the RSS cost of each. Pick the rule closest to optimal with least memory. Also sanity
-check the `c ≤ 32 ⇒ mc ≤ 128` cap against where the knee actually sits.

### Step 4 — Repeat for verify (where it matters most)
Verify has different `f`, real CPU work, and the bandwidth / single-2.36 GB-bucket
long-pole limiter — so the existence-optimal mapping may not transfer. The design
must hold up here. Watch median SRTT + minRTT for genuine bandwidth-saturation RTT
rise (App C caveat in the progress report).

### Step 5 (optional) — RTT sensitivity
Using the local-HTTP-sim with `tc netem`, sweep injected RTT and confirm the knee
**moves with latency** — direct evidence that optimal-`mc` is backend-determined,
which would settle §3 and argue for the inverse derivation (or a backend-aware `mc`).

## 5. Where to run it (load consideration)

- **Existence (Steps 1–3):** cheap (no payload) → run on the public archive
  `history.stellar.org` directly, polite ascending ramp, retry hard-stop.
- **Verify (Step 4) + RTT sensitivity (Step 5):** a 36-cell grid downloading GB per
  cell is ~150 GB of repeated downloads — **do NOT hammer the public archive.** Use
  the parked **local-HTTP-sim** (`docs/max-concurrent-local-http-sim.md`): serve the
  local 7.2 TB mirror over a concurrent HTTP server, no politeness limits, and
  `tc netem` for RTT injection.

## 6. Deliverables

1. A measured (c, mc) throughput surface for existence and verify (heatmaps + CSVs).
2. The effective fan-out `f` per mode.
3. A scored comparison of the candidate mappings (throughput gap + RSS cost).
4. **A recommended single-knob design**: which knob to expose, the derivation
   formula, the bounds (`c_max`, `mc_max`), and whether `mc` should be backend-aware.
5. If adopted: a follow-up implementation issue (CLI change + default derivation +
   docs), kept separate from this measurement work.

## 7. Open decisions (resolve before running)

1. Run existence on public + verify on local-sim (recommended), or everything on one
   target?
2. Grid resolution — is 6×6 enough, or add `c=64`, `mc=512` to see the regression
   edge?
3. Do we want Step 5 (RTT sweep) in scope now, or only if Steps 1–4 are ambiguous?
4. Is the single-knob change actually desired as a shipped feature, or is this purely
   to derive a better *default* for the existing two knobs? (Changes deliverable #5.)
