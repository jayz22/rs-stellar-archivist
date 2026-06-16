# HANDOFF — verify CPU-scaling experiment (perf-verify-speedup)

> **Purpose:** full session state so a fresh Claude Code session — ideally on a
> **dedicated, quiet machine** — can resume and run clean measurements. Written
> 2026-06-16. Branch `perf-verify-speedup` (worktree
> `../rs-stellar-archivist-verifyperf`, off `perf-testing`).

## ⚠️ Why we stopped: measurement contamination

Every measurement in this session ran on a box that was **simultaneously
running a Stage 2.2 full-pubnet mirror** (PID 1368832, `sa-perf mirror
https://history.stellar.org/.../core_live_001 file:///data/pubnet-mirror -c 32
--skip-optional`, started ~05:17, 13h+ elapsed, ~1.6 TB written). It consumes
~0.35–1 CPU core + memory bandwidth + ~100 MB/s network + ~100 MB/s disk-write.

Consequence: absolute numbers (core-utilization, the ~380 MB/s plateau) and the
**Phase 2 "sync didn't help" conclusion are NOT trustworthy** — a shared
ceiling (memory bandwidth or the competing load) could mask a real sync benefit.
**All Phase 0/1/2 numbers below must be re-measured on a quiet box** before any
conclusion is final. (The contamination also polluted the mirror's own Stage 2.2
throughput numbers — note for that effort.)

The user chose: **do NOT disturb the mirror**; commit everything and move to a
dedicated machine. That is what this doc supports.

## Goal

Make `--verify` (scan/mirror) actually use the box's many cores instead of
plateauing. Focus order: **time/CPU first, memory second.** Hard correctness
gate every run: full-fixture `scan --verify` exits 0 with `summary.failed == 0`.

## The workload & why it's hard (this part is solid, not contamination-sensitive)

- Fixture: recent 2,000-checkpoint pubnet window, `file:///data/perf/fixture`,
  55 GB / 26,464 files, ~183 GB decompressed. Bounds `--low 62918015 --high 63046015`.
- Verify self-time is ~92% gzip-decompress + SHA-256 (`bucket_stream` 70–83% +
  `xdr_decompress` 11–21%; raw `phases.csv` in `perf-results/scanverify_C4_r1/`
  on the main checkout). SHA-256 is HW-accelerated (ARMv8, `sha2`+`cpufeatures`)
  → dominant real cost is **gzip inflate** (miniz_oxide).
- **Byte skew is extreme:** of 39.9 GB buckets (16,459 files), the largest single
  bucket is **2.36 GB compressed**, next several ~480 MB, p50 0.5 MB. A single
  gzip stream can't be parallelized → the big bucket is a **serial long-pole**.
  Bucket dedup (LRU) means steady-state checkpoints add few *new* buckets, so the
  amount of independent decode work may be small — a likely real limiter,
  independent of decode implementation.

## What was done, phase by phase

### Phase 0 — baseline (COMMITTED 34ecd4d) — DONE, but re-measure clean
Calibrated to Stage 2.1 within ~1%. Numbers (warm, `sa-perf`, 1 rep):
| -c | wall | peak RSS | MB/s |
|----|------|----------|------|
| 1  | 841.4s | 1120.7 | 217.6 |
| 4  | 480.1s | 1354.9 | 381.4 |
| 16 | 486.6s | 2059.5 | 376.3 |
`probe.sh` @c16 said ~0.6 cores — **but `bottleneck.sh` later showed 3–5 cores**;
trust `bottleneck.sh` (probe.sh's proc-CPU read was misleading).

### Phase 1 — gzip backend swap (COMMITTED 6e5e58e) — DONE, re-measure clean
Added experiment cargo features `fast-zlib-rs` / `fast-zlib-cf` / `fast-zlib-ng`
(re-export flate2 backends; flips async-compression's shared GzipDecoder too via
feature unification). Backend wiring proven in
`perf-results/verifyperf/env/backends.txt`.
Result: zlib-ng vs miniz → **c=1 −6.5%** (841→787s), **c=4/16 ~0%**; all backends
within ~3% at c=32. Conclusion (tentative, contaminated): backend swap is a
modest single-stream win that vanishes under concurrency.

### Phase 2 — synchronous blocking decode (UNCOMMITTED until this handoff)
Reworked the **scan-verify bucket path only** in `src/verify.rs`:
`verify_bucket_stream` now uses `tokio::task::spawn_blocking` + a `SyncIoBridge`
over `reader.into_futures_async_read(..).compat()` → `flate2::read::GzDecoder` +
SHA-256 in a tight sync loop. No mpsc channel, no per-64 KB await; still streams
(no whole-file buffering). Mirror-write path (`verify_bucket_maybe_write` /
`verify_and_write_bucket`) and the `xdr_verify.rs` decode are **unchanged**.
Cargo.toml: added `io-util` to tokio-util features (gates `SyncIoBridge`).

Correctness: ✅ smoke 6492 files failed=0; ✅ 25 unit tests inc.
`bucket_hash_mismatch`, `bucket_invalid_gzip`, `detects_corrupt_files`, `empty_bucket`.

Result (CONTAMINATED): wall basically unchanged vs baseline (c1 845.5, c4 478.3,
c16 483.3); **c=1 peak RSS halved (1121→561 MB)** — a real memory win. Core
utilization ~unchanged. `bottleneck.sh` @c16 verdict: not CPU/disk/net bound;
**~110–170 threads parked on `futex_wait_queue`, only 2–6 running** → a
concurrency/lock stall (could be SyncIoBridge `block_on` per read, or the
workload's intrinsic dedup/long-pole limit, or the competing mirror — undecided
under contamination).

## Binaries on the OLD box (rebuild on the new one)
`bin/sa-perf` (miniz baseline), `sa-perf-zrs/-cf/-zng` (Phase 1 backends),
`sa-perf-sync` (Phase 2 sync, miniz). All `--features perf-metrics`. Build recipe
in `perf-results/verifyperf/ARTIFACTS.md`.

## NEXT STEPS on the dedicated/quiet box
1. Confirm the box is quiet (`scripts/perf/bottleneck.sh` with nothing running;
   no mirror, no `du`). Put fixture on fast local disk; warm it.
2. Build baseline `sa-perf`, `sa-perf-sync`, `sa-perf-zng` (+ a `sync+zng`
   combo: `--features perf-metrics,fast-zlib-ng` with the Phase 2 code).
3. Re-run `scan --verify` c=1/4/16 (1 rep) for each; sample `bottleneck.sh` at
   c=16 for EACH variant (the key "did we use the cores" evidence).
4. **Decide Phase 2** on clean data: did sync raise cores/throughput once the
   competing load is gone? If still futex-bound, investigate: is it SyncIoBridge
   `block_on` overhead (try buffering compressed bytes then decoding, or a larger
   `SYNC_READ_BUF`), or the intrinsic dedup/big-bucket limit (then the answer is
   "verify is long-pole-bound; -c≈4 is optimal; document it")?
5. Then proceed to the max-concurrent experiment (`docs/max-concurrent-sweep-plan.md`,
   a REMOTE experiment — needs network, not the local fixture; 3 open decisions
   in its §10 still unanswered).

## Artifacts (all preserved on the OLD box, committed as text here)
`perf-results/verifyperf/` — see its `ARTIFACTS.md` index. Includes per-run dirs
(baseline/, zlib-ng/, sync-miniz/), smoke/, env/ (build logs + backend proof),
logs/, diag/ (the `bottleneck.sh` run that found the contamination is in
`logs/`/the diag dir; the contaminating mirror was identified via `ps`).
Policy: keep everything; one `SA_PERF_OUT` dir per run; never `SA_PERF_OUT=""`.

## Open commits / state
- Committed: Phase 0 (34ecd4d), Phase 1 (6e5e58e), merge of perf-testing (10bbb87).
- This handoff commits: Phase 2 code, the max-concurrent plan, this doc, and the
  verifyperf text artifacts (force-added past the perf-results gitignore so they
  travel with the branch).
