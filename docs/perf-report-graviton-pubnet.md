# Stellar Archivist — Performance & Correctness Report (Linux / Graviton, pubnet)

> **Status: LIVE — being filled in as runs complete.** Sections marked ⏳ are
> placeholders that get populated when their stage finishes. This report covers
> the **Ubuntu/aarch64 (AWS Graviton2)** host using a **recent pubnet** fixture.
> A separate earlier report (`docs/perf-report.md`) covers the macOS/testnet run.

Last updated: 2026-06-15 (sweep in progress).

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

## Stage 2.1 — Concurrency scaling (§6.1) — ⏳ IN PROGRESS

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

### Wall time (ms) vs `-c` — ⏳ pending sweep completion

| mode | c=1 | c=2 | c=4 | c=8 | c=16 | c=32 | c=64 |
|---|---|---|---|---|---|---|---|
| scan           | _…_ | | | | | | |
| scan-verify    | _…_ | | | | | | |
| mirror         | _…_ | | | | | | |
| mirror-verify  | _…_ | | | | | | |

### Peak RSS (MB) vs `-c` — ⏳ pending

| mode | c=1 | c=2 | c=4 | c=8 | c=16 | c=32 | c=64 |
|---|---|---|---|---|---|---|---|
| scan           | | | | | | | |
| scan-verify    | | | | | | | |
| mirror         | | | | | | | |
| mirror-verify  | | | | | | | |

### Throughput & phase breakdown — ⏳ pending

(Plots: `perf-results/plots/time_vs_concurrency.png`,
`rss_vs_concurrency.png`, `phase_breakdown.png`.)

### Early single-threaded (`-c=1`) reference points

From validation/early runs on this fixture (warm cache):
- `scan` (existence only): ~0.8 s, ~44 MB RSS — `stat` of 26,464 files; no decompress/hash.
- `scan --verify`: ~14 min, ~1.1 GB RSS, ~219 MB/s over ~183 GB decompressed.

*(These will be superseded by the final median table above.)*

### Findings — ⏳ pending

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
