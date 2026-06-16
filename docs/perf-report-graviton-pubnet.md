# Stellar Archivist — Performance & Correctness Report (Linux / Graviton, pubnet)

> **Status: LIVE — being filled in as runs complete.** Sections marked ⏳ are
> placeholders that get populated when their stage finishes. This report covers
> the **Ubuntu/aarch64 (AWS Graviton2)** host using a **recent pubnet** fixture.
> A separate earlier report (`docs/perf-report.md`) covers the macOS/testnet run.

Last updated: 2026-06-16 (Stage 2.1 complete; Stage 2.2 mirror starting).

---

## Environment

| | |
|---|---|
| Host | `user-dev-007` (AWS EC2) |
| CPU | ARM **Neoverse-N1** (Graviton2), **32 vCPU** (1 thread/core) |
| RAM | 123 GiB (118 GiB avail) |
| OS | Ubuntu, Linux 6.14.0-1018-aws, **aarch64** |
| rustc | 1.96.0 (ac68faa2 0, 2026-05-25) |
| Data disk | `/data` = **RAID0 (md0) over 2× 6.8 TB EC2 NVMe instance-store**, ext4, `noatime`, 14 TB |
| Root disk | `/dev/root` ext4, ~96 GB EBS (NOT used for fixtures/measurements) |
| Network → pubnet archive | ~203 MB/s single-stream (500 MB range probe) |
| Binaries | `bin/sa-clean` (release), `bin/sa-perf` (release + `perf-metrics`), `bin/corrupt-archive` |

Wall + peak RSS are taken from the in-process instrumentation (`sa-perf`) and
cross-checked against GNU `/usr/bin/time -v` (`peak_rss_mb_os`). Raw artifacts
live under `perf-results/` (gitignored): per-run dirs (`cmd.txt`, `stdout.log`,
`stderr.log`, `report.json`), `summary.csv`, and `plots/`.

---

## Stage 1 — Correctness (§5) — ✅ ALL PASS

Run on the local testnet fixtures (`file://`), v1 and v2, via
`scripts/perf/stage1.sh`. Artifacts under `perf-results/stage1/{v1,v2}/`.

| Check | v1 (`testnet-archive-small`) | v2 (`testnet-archive-v2`, hotArchiveBuckets) |
|---|---|---|
| §5.1 scan `--verify` clean + inventory match | ✅ **1954 == 1954** files, 0 broken | ✅ **3117 == 3117** files, 0 broken |
| §5.2 mirror `--verify` → `diff -r` vs src | ✅ empty (well-known rewrite rule asserted) | ✅ empty |
| §5.3 corrupt(20, seed 1) → detect → repair → `diff` | ✅ snapshot restored (diff empty) | ✅ snapshot restored |
| §5.4 corrupt(10, seed 2) → dry-run `--verify` plan → apply `--plan` → `diff` | ✅ snapshot restored | ✅ snapshot restored |

Notes: the v2 fixture is rebuilt from the **genesis** testnet range (so a
full-range scan is self-consistent). Repair detection is asserted via the
multi-section report's `main_pass.checkpoints` (content/chain damage) plus the
byte-level `diff -r` against the pre-corruption snapshot; file-level damage is
repaired inline and is confirmed by the empty diff rather than the report's
failure set. See `scripts/perf/check_stage1.py`.

---

## Stage 2.1 — Concurrency scaling (§6.1) — ✅ COMPLETE

**Fixture:** a **recent** 2,000-checkpoint pubnet window, mirrored locally and
used read-only as `file://`.

| | |
|---|---|
| Source | `https://history.stellar.org/prd/core-live/core_live_001` |
| Checkpoint range | ledgers **62,918,015 → 63,046,015** (2,001 checkpoints) |
| Format | version 2 (hotArchiveBuckets), tip ledger 63,046,015 |
| Size on disk | **55 GB**, 26,464 files |
| Decompressed payload | ~183 GB (gz → raw, from the verify pass) |
| Location | `/data/perf/fixture` |
| Cache | pre-warmed (results are **warm-cache**: engine CPU/IO, not cold disk) |

**Sweep:** `-c ∈ {1,2,4,8,16,32,64}` × **4 modes** (`scan`, `scan --verify`,
`mirror`, `mirror --verify`) × **3 reps** = **84 runs**. Every op bounded with
`--low 62918015 --high 63046015` (the fixture is partial; its `.well-known`
advertises the tip). Repair is excluded from the scaling sweep by design (see
plan §6.1). Values reported as **median (min–max)** across reps.

**84/84 runs completed, all exit 0.** Values are **median (min–max)** of 3 reps.

### Wall time (s) vs `-c`

| mode | c=1 | c=2 | c=4 | c=8 | c=16 | c=32 | c=64 |
|---|---|---|---|---|---|---|---|
| scan          | 0.8 | 0.6 | 0.7 | 0.7 | 0.7 | 0.7 | 0.7 |
| scan-verify   | **836.2** | 530.3 | **476.5** | 478.2 | 483.0 | 487.9 | 496.4 |
| mirror        | 30.4 | 25.2 | **23.2** | 23.2 | 24.4 | 23.7 | 23.4 |
| mirror-verify | **853.6** | 550.0 | **506.0** | 509.4 | 511.5 | 518.9 | 527.9 |

### Peak RSS (MB) vs `-c`

| mode | c=1 | c=2 | c=4 | c=8 | c=16 | c=32 | c=64 |
|---|---|---|---|---|---|---|---|
| scan          | 43 | 44 | 44 | 43 | 46 | 47 | 52 |
| scan-verify   | 1114 | 1313 | 1553 | 1660 | 1986 | 2032 | **2108** |
| mirror        | 227 | 347 | 448 | 498 | 627 | 857 | **923** |
| mirror-verify | 1097 | 1364 | 1367 | 1610 | 1708 | 1716 | **1844** |

Plots: `perf-results/plots/time_vs_concurrency.png`, `rss_vs_concurrency.png`,
`phase_breakdown.png`.

### Knee / plateau (best `-c` by min-median wall)

| mode | best `-c` | wall @ best | wall @ c=1 | speedup c1→best |
|---|---|---|---|---|
| scan          | 2 | 0.6 s   | 0.8 s   | 1.24× |
| scan-verify   | 4 | 476.5 s | 836.2 s | **1.76×** |
| mirror        | 4 | 23.2 s  | 30.4 s  | 1.31× |
| mirror-verify | 4 | 506.0 s | 853.6 s | **1.69×** |

### Phase self-time % at the plateau (`-c=4`)

| mode | bucket_stream | xdr_decompress | xdr_parse_tx | xdr_parse_result | (gzip+hash total) |
|---|---|---|---|---|---|
| scan-verify   | 70.3 | 21.3 | 6.6 | 1.6 | **91.6%** |
| mirror-verify | 82.6 | 11.1 | 4.9 | 1.2 | **93.7%** |

(`scan` and `mirror` without `--verify` spend ~0% in these phases — existence
`scan` is `stat`-bound; plain `mirror` is ~100% the `copy` phase. Phase times are
self-time summed across concurrent tasks and **overlap wall-clock** — read as
"where the work is," per §2.4.)

### Findings

1. **Concurrency plateaus at `-c=4`, then mildly *regresses*.** All modes bottom
   out around `-c=4` and get slightly *worse* toward `-c=64` (scan-verify
   476→496 s; mirror-verify 506→528 s). So past 4 in-flight checkpoints, added
   concurrency only adds overhead/contention on this workload — it does **not**
   use the 32 cores. Best total speedup is just **1.76×** (scan-verify, c1→c4).
2. **Verify is the only thing that scales at all; existence-scan and mirror are
   IO/stat-bound** and basically flat (1.2–1.3×).
3. **Verify cost is overwhelmingly gzip-decompress + SHA-256.** At the plateau,
   `bucket_stream` + `xdr_decompress` = **~92%** (scan-verify) / **~94%**
   (mirror-verify); XDR decode/parse and the cross-file/chain checks are a few
   percent. Buckets dominate — pubnet's current bucket state is the bulk of the
   bytes. Verify throughput peaks ≈ **384 MB/s** over ~183 GB decompressed.
4. **Peak RSS grows steeply with `-c`** — the concurrency/memory trade-off:
   scan-verify **1.1 GB → 2.1 GB**, mirror **0.23 GB → 0.92 GB**, mirror-verify
   **1.1 GB → 1.8 GB** from c=1→64. So raising `-c` past the c=4 knee costs
   memory **and** wall time here — strictly worse on local data.
5. **Practical takeaway:** for local/warm work, `-c≈4` is optimal on this
   32-core box; the default `-c=32` trades ~2× RSS for ~no speed (slightly
   slower). `-c`'s real value is latency-hiding for **remote** archives — which
   §6.2 exercises.

> **Note vs the earlier macOS/testnet run** (`docs/perf-report.md`): that run
> plateaued even earlier (~c=2) on a tiny fixture; here, with a realistic 55 GB
> pubnet fixture on 32 cores, the knee is c=4 and the post-knee regression is
> clearer. Both agree the engine saturates with a handful of in-flight
> checkpoints and does not scale across many cores on local data.

---

## Stage 2.2 — Full pubnet (§6.2) — ⏳ NOT STARTED

Planned: full genesis→tip mirror to `/data/pubnet-mirror` (`-c 32`), then remote
scan / scan-verify, repair from remote, and a local-engine re-run for the
network-free comparison. Network is ~203 MB/s (single-stream) — runs will be
network-bound; bandwidth noted alongside results.

**Goals:** (1) throughput & memory at *true* archive scale (TBs), not a warm
55 GB fixture; (2) **how network-bound each op is** — same op run remote vs
against the local mirror, the delta isolates network from engine; (3) repair
perf at scale (excluded from the §6.1 sweep).

**Network/local map:** local = the mirror output, the corrupted copy, and the
repair *target*. Network-bound = the mirror *download* (step 1), the remote
scans (step 2), and the repair *re-fetch source* (step 3, repairs a local copy
by pulling good files from the remote). Step 4 repeats scan-verify + repair with
a **local** source to strip the network out (the control).

**Locked decision — `--skip-optional` on every full-pubnet op** (mirror, scan,
scan-verify, repair). Pubnet did not archive SCP before **ledger 1,214,079**
(checkpoint `0x0012867f`, closed **2015-12-07**, protocol 1) — below that,
`scp-*.xdr.gz` is HTTP 404. Confirmed engine behavior: scan/mirror do **not**
abort on a 404; they process all checkpoints concurrently, record each missing
file, and return a non-zero exit only at the end. `--skip-optional` never
requests SCP, so the full genesis→tip range runs clean. Trade-off: no SCP in the
mirror and no at-scale `XdrParseScp` timing (SCP correctness is covered in
Stage 1). Verify is **OFF for the step-1 mirror** (it measures download+write
throughput; verify cost comes from the scan-verify steps).

---

## Measurement caveats (apply throughout)

- **Phase times overlap** (concurrent checkpoints + spawned tasks), so they sum
  to more than wall-clock; read phase **percentages** as "where the work is,"
  not as durations (plan §2.4). Wall time and peak RSS are the authoritative
  absolute numbers.
- **Existence-scan throughput is misleading**: `mb_per_s` divides by the
  history-JSON bytes read (~12 MB), ignoring the 55 GB payload existence-scan
  never touches. The real existence-scan cost is the `stat` count, not bytes.
- **Warm cache** for §6.1 (isolates the engine); cold-disk / network-bound
  behavior is what §6.2 measures.

---

## Issues found & fixed during this run

- **Genesis fixture build failed (exit 2):** early pubnet predates SCP
  archival, so `scp-*` 404s aborted the mirror. Fixed by using the recent
  2,000-checkpoint window (plan §6.1 note added).
- **Unbounded scan of a partial fixture (exit ≠ 0, 2 GB error log):** the
  fixture's `.well-known` advertises the tip, so an unbounded scan tried the
  full genesis→tip range and errored on ~983k missing checkpoints. Fixed by
  passing `--low/--high` on every fixture op (now built into `scaling.sh`).
- **Harness:** `run.sh` now auto-saves `report.json` per run alongside the logs.
