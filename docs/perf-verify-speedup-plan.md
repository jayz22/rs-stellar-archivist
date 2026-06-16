# Verify Throughput / CPU-Scaling Improvement — Experiment Plan

> **Branch / worktree:** `perf-verify-speedup`, in the dedicated worktree
> `../rs-stellar-archivist-verifyperf` (created off `perf-testing`). **All code,
> build, and result artifacts for this effort stay inside this worktree.** The
> `/data/perf/fixture` archive is used **read-only**.
>
> **Status:** DRAFT — awaiting approval before any implementation.

---

## 1. Goal & success criteria

**Primary:** improve **wall time and CPU utilization** of the `--verify` path so
it actually uses the box's 32 cores instead of plateauing at ~4. **Secondary
(later):** peak RSS.

Concrete targets (on the existing §6.1 fixture, warm cache, this Graviton2 box):

| Metric | Baseline (Stage 2.1) | Target |
|---|---|---|
| scan-verify wall @ best `-c` | 476.5 s (c=4) | materially lower |
| scan-verify single-stream (c=1) | 836.2 s | lower (per-core gzip ceiling) |
| Speedup c=1→plateau | 1.76× | higher, knee moves right of c=4 |
| Verify throughput (decompressed) | ~384 MB/s | higher |

**Non-negotiable correctness gate:** every variant must still verify the fixture
**cleanly** — `scan --verify` over the full fixture exits 0 with **0 broken**
items in `report.json` (a wrong gzip/hash result would surface as a hash
mismatch failure). This is checked on every measured run, plus a Stage-1 spot
check (`scripts/perf/stage1.sh`) for the winning variant.

---

## 2. Diagnosis (evidence already gathered)

1. **Verify is ~92% gzip-decompress + SHA-256** (`BucketStream` 70–83%,
   `XdrDecompress` 11–21%; phase table, report §2.1). SHA-256 is **hardware
   accelerated** on this Neoverse-N1 (`sha2` + `cpufeatures` → ARMv8 crypto), so
   the dominant real cost is **gzip inflate**.
2. **The gzip backend is `miniz_oxide`** (pure-Rust; confirmed the only zlib
   impl in `Cargo.lock`). It is ~2–3× slower per core than zlib-ng / zlib-rs.
   Aggregate verify throughput peaks at **~384 MB/s ≈ one core of miniz_oxide
   inflate**.
3. **The bytes are brutally skewed.** Of 39.9 GB of buckets (16,459 files), the
   single largest is **2.36 GB compressed**; the next several ~480 MB; p50 is
   0.5 MB. A **single gzip stream cannot be decoded across cores**, so each big
   bucket runs start-to-finish on one core. There are only a handful of big
   ones, so beyond ~4 in-flight checkpoints there is nothing left to overlap —
   the 2.36 GB bucket is a **serial long-pole** that floors wall time. Extra
   `-c` past 4 only adds RSS + channel/scheduler contention → the observed mild
   regression.
4. **The decode path is async + chunked + channel-bridged** (`verify.rs`,
   `xdr_verify.rs`): 64 KB reads each behind an `.await`, shipped one `Bytes` at
   a time through a 64-cap mpsc channel to a spawned hash task. That is heavy
   per-byte async overhead for pure-CPU work, and it keeps the CPU work on the
   async executor.

**Implication.** Two independent levers, both about *time/CPU*:

- **Lever A — faster gzip backend** (raises the single-core ceiling; directly
  shrinks the serial long-pole). Biggest, lowest-risk win. Config-only change.
- **Lever B — run decode+hash synchronously on a blocking thread** (drop the
  per-chunk channel/await overhead; let the OS schedule concurrent decodes
  across all cores). Architectural change to `verify.rs` / `xdr_verify.rs`.

The irreducible floor that remains after both: one 2.36 GB gzip member is
sequential by nature; Lever A is what makes that member faster.

---

## 3. Methodology / controls (read first)

- **Fixture:** existing `file:///data/perf/fixture` (recent 2,000-cp pubnet
  window, 55 GB / 26,464 files), **read-only**. Every op bounded with
  `--low 62918015 --high 63046015` (the fixture is partial).
- **Warm cache:** pre-warm once (`cat`/`vmtouch`-style read) before each
  variant's runs so we measure engine CPU, not cold disk — matches Stage 2.1.
- **Harness:** reuse `scripts/perf/run.sh` (OS `/usr/bin/time -v` wall + peak
  RSS, in-process `sa-perf` cross-check, auto-saved `report.json`). Results go
  to **this worktree's** `perf-results/verifyperf/` (kept separate from the
  Stage 2.1 artifacts in the main checkout).
- **CPU-utilization probe:** during one run per variant, sample
  `scripts/perf/probe.sh` (system CPU %, cores-equivalent, loadavg) to *directly*
  show whether we moved off the ~1-core ceiling. This is the headline "did it
  use the cores" evidence.
- **Cells (subset re-sweep, per your call):** `scan --verify` at **c=1, 4, 16**.
  - c=1 isolates the **per-core gzip** speedup (long-pole proxy).
  - c=4 is the **old plateau**.
  - c=16 shows whether a faster per-core rate **moves the knee right** / lifts
    the multi-core ceiling.
- **Reps:** **1 per cell.** This experiment only needs a *strong speedup
  signal*, not a tight CI. Stage 2.1 showed <2% rep variance on scan-verify, and
  the Phase 0 calibration reproduced the recorded c=1 median to within ~0.6%
  (841.4 s vs 836.2 s) — a 2–3× backend win dwarfs that noise. (Methodology
  changed from 2 reps after the calibration confirmed how low the variance is.)
- **Binaries:** built **in this worktree** with the production release profile.
  `sa-clean` (headline wall/RSS) and `sa-perf` (phase split). Backends selected
  by cargo feature so all variants come from one source tree (see §4).
- **What stays fixed across variants:** fixture, bounds, cache state, host,
  rustc, harness, `--max-concurrent` default. Only the gzip backend (Phase 1) or
  the decode structure (Phase 2) changes.

---

## 4. Backend selection mechanism (Phase 1 plumbing)

Add cargo features that re-export flate2 backends, so a single source tree
builds every variant (no source edits needed to switch gzip libs). flate2 is a
direct dependency and async-compression's `GzipDecoder` shares the **same**
flate2 via feature unification, so flipping the backend flips both the bucket
and XDR decode paths at once. Verified backend priority in flate2 1.1.9:
`any_c_zlib` > `zlib-rs` (sets `any_zlib`, disables miniz) > `miniz_oxide`.

```toml
[features]
fast-zlib-rs = ["flate2/zlib-rs"]        # pure-Rust zlib-ng port; NO C toolchain
fast-zlib-cf = ["flate2/cloudflare_zlib"] # C (cc/gcc — present); no cmake
fast-zlib-ng = ["flate2/zlib-ng"]         # C zlib-ng; needs cmake (NOT installed)
```

- **Baseline:** plain build → `miniz_oxide`.
- **`fast-zlib-rs`:** pure-Rust, builds today, *mergeable later* (no new system
  deps). Primary candidate.
- **`fast-zlib-cf`:** C backend, cc-only — second data point, no cmake needed.
- **`fast-zlib-ng`:** the canonical fast C backend, **blocked on cmake**
  (`sudo apt-get install -y cmake`, password required). Run only if you install
  cmake; otherwise `zlib-rs` is the zlib-ng-class representative.

These features are **perf-experiment only** (this branch); they do not change
the default build.

---

## 5. Phased execution

Each phase ends with a **written results + conclusion summary presented to you**;
I **commit only after your approval**, then proceed to the next phase.

### Phase 0 — Baseline calibration & probe (no code change)
- Pre-warm cache; build `sa-clean`/`sa-perf` in the worktree from current source.
- Run `scan --verify` at c=1, 4, 16 (2 reps) → confirm the numbers reproduce the
  Stage 2.1 medians (controls for machine drift since the original sweep).
- Run `probe.sh` mid-run at c=16 → record current cores-equivalent (expected
  ~1–2). This is the "before" CPU-utilization datapoint.
- **Deliverable:** a baseline table + probe snapshot. (Nothing to commit beyond
  the plan; calibration artifacts saved under `perf-results/verifyperf/baseline/`.)

### Phase 1 — gzip backend swap (Lever A)
- Add the §4 features to `Cargo.toml` (worktree only).
- Build `fast-zlib-rs` and `fast-zlib-cf` variants (+ `fast-zlib-ng` iff cmake).
- Correctness gate: each variant's scan-verify exits 0, `report.json` 0 broken.
- Measure scan-verify c=1, 4, 16 (2 reps) per variant; `probe.sh` at c=16.
- **Deliverable / conclusion:** head-to-head table (wall, RSS, throughput,
  cores-equiv) baseline vs each backend; pick the winner; state the speedup and
  whether the knee moved. → approval → commit.

### Phase 2 — synchronous blocking decode+hash (Lever B)
- Rework `verify.rs::verify_bucket_maybe_write` (and the analogous
  `xdr_verify.rs` decode path) to do decode + SHA-256 in a **`spawn_blocking`**
  task with a tight read loop over a large buffer, removing the per-chunk mpsc
  channel and per-64 KB `.await`. Mirror-verify still streams compressed bytes
  to dst (write path preserved); memory kept bounded by reading incrementally,
  not buffering whole multi-GB files.
- Keep the winning backend from Phase 1.
- Correctness gate + measure the same cells + probe.
- **Deliverable / conclusion:** incremental effect over Phase 1 (overhead
  removed? cores-equiv up? throughput up?). → approval → commit.
- *Risk note:* this is real engine code; if the win is marginal over Phase 1 or
  the complexity/memory cost is high, we may stop at Phase 1. Decided with data.

### Phase 3 — (optional) memory & wrap-up
- Revisit peak RSS for the chosen design across c=1→16 (the secondary goal).
- Update `docs/perf-report-graviton-pubnet.md` Stage 2.1 with a "verify
  optimization" addendum (before/after), and note any residual long-pole floor.
- **Deliverable:** final summary; decide what (if anything) is worth proposing
  back to the engine on a non-experimental branch.

---

## 6. Out of scope / explicitly not doing

- Parallelizing a **single** gzip member across cores (impossible for one
  stream; would require the producer/stellar-core to shard buckets).
- Touching the default production build, or merging anything off this branch.
- Re-running the full 84-run §6.1 sweep (subset only, per your call) — unless a
  result warrants the full apples-to-apples confirmation at the end.
- Any write to `/data/perf/fixture` (read-only source).

---

## 7. Estimated run cost (rough, warm cache)

Per scan-verify run ≈ 183 GB decompressed ÷ throughput. Baseline ≈ 14 min (c=1),
8 min (c=4/16). A faster backend should cut these. Per-phase budget (2 reps ×
3 cells): baseline ~1 h; each backend variant ~0.5–1 h. Runs are backgrounded;
I'll checkpoint progress and not block on them.
