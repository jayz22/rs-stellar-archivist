# Stellar Archivist — Performance & Correctness Testing Plan

> **Audience:** an engineer/agent who can build and run Rust and shell, but who does
> **not** need deep knowledge of the archivist internals. Everything needed is here.
>
> **Primary goal:** measure **time, memory (peak RSS), and throughput** of each
> operation/mode, plus a **per-phase breakdown** of where time goes (IO/decompress
> vs XDR parse vs hashing vs verification vs write). **Secondary goal:** verify
> correctness (all files scanned; a mirror reproduces src; a repair restores src).

---

## 0. Decisions locked for this effort

| Decision | Choice |
|---|---|
| Full-range target | **True full pubnet** (genesis → tip). Large: budget hundreds of GB of disk and many hours. |
| In-process instrumentation | Custom, **`perf-metrics` cargo feature**, off by default. **Kept on the `perf-testing` branch, NOT merged into the repair PR.** |
| Metrics output | **CSV + Python/matplotlib**. No Prometheus. Time-series RSS via the harness polling `ps` (no code). |
| Corruption tooling | **Rust tool, full menu** (feature-gated `[[bin]]`), reusing the proven test corruption helpers. |
| Where it lives | Branch **`perf-testing`** off `repair-v2`: this doc, `scripts/perf/`, the gated instrumentation, the corruption bin. Raw results in **gitignored** `perf-results/`. |

---

## 1. Background you need (archivist in 5 minutes)

- **Binary:** `stellar-archivist`. Build: `cargo build --release` → `target/release/stellar-archivist`.
- **Operations:** `scan <archive>`, `mirror <src> <dst>`, `repair <src> <dst>`. Sources/dests are URLs: `file://…`, `https://…`, `s3://…`, etc.
- **An archive** is laid out by *checkpoint* (every 64 ledgers: 63, 127, 191, …). Per checkpoint there are up to 5 files — `history-*.json`, `ledger-*.xdr.gz`, `transactions-*.xdr.gz`, `results-*.xdr.gz`, and optional `scp-*.xdr.gz` — plus content-addressed `bucket/…/bucket-<hash>.xdr.gz` (deduplicated across checkpoints). The root `.well-known/stellar-history.json` describes the archive's current state.
- **Key flags** (all global unless noted):
  - `-c, --concurrency <N>` — concurrent checkpoints (default **32**). This is the knob the scaling test sweeps. (The tokio runtime is already multi-threaded; `-c` bounds in-flight checkpoints, not OS threads.) **Note: the short flag is lowercase `-c`.**
  - `--max-concurrent <N>` — concurrent I/O ops per backend (default 64).
  - `--verify` — download + decompress + hash/verify content (XDR + bucket SHA256). Without it, **scan only checks existence**; mirror/repair copy bytes without verifying.
  - `--report <path>` — write a JSON status report (counts + the broken-file set).
  - `--skip-optional` — skip SCP files.
  - `--low <ledger> / --high <ledger>` — restrict the checkpoint range (scan/mirror/repair).
  - `--overwrite`, `--allow-mirror-gaps` (mirror); `--dry-run`, `--plan <file>` (repair).
  - `--debug` / `--trace` — logging. Leave **off** for perf runs (logging adds overhead/noise).
- **Exit code:** non-zero on operation failure (e.g., missing/corrupt files that couldn't be handled). The harness treats non-zero as run failure.

### Two facts that drive the correctness checks

1. **`--verify` checks** (what a corruption must be able to trip): bucket SHA256 (decompressed content vs filename hash); ledger-header hash (`SHA256(header) == entry.hash`); checkpoint completeness (all expected ledger seqs present); transaction-set hash cross-check (header's `tx_set_hash` vs computed); result-set hash cross-check; intra-checkpoint hash chain (`ledger[N].prev == hash(ledger[N-1])`); cross-checkpoint chain (first ledger of cp N vs last of cp N-1); plus plain existence and gzip/JSON validity.
2. **Mirror and repair REWRITE `.well-known/stellar-history.json`** — they copy the **highest mirrored checkpoint's** `history-*.json` into `.well-known/` rather than copying src's `.well-known` byte-for-byte. For a **complete** archive mirrored over its **full** range, src's `.well-known` already equals its highest history file, so dst ends up byte-identical to src everywhere. For **partial** ranges, dst's `.well-known` reflects dst's highest checkpoint, not src's. The correctness checks below account for this.

---

## 2. Instrumentation spec (`perf-metrics` feature)

A `metrics` module compiled **only** under `--features perf-metrics`. When the feature is off there is no code (macros expand to `()`), so production/PR builds are unaffected.

### 2.1 Phases (aggregate counters)

Each phase holds `{ nanos, calls, bytes }` as `AtomicU64`. A phase is entered with an RAII guard placed at a code boundary; on drop it adds `elapsed` (and the caller adds `bytes` where meaningful).

| Phase | Wraps | Nature | bytes |
|---|---|---|---|
| `HistoryFetch` | download of `history-*.json` | IO | ✓ (compressed/json size) |
| `HistoryParse` | JSON deserialize + `validate()` | CPU (clean) | |
| `BucketStream` | bucket gzip-decompress **+ SHA256** (streamed; +write in mirror-verify) | interleaved bundle | ✓ (decompressed) |
| `XdrDecompress` | gzip-decompress of ledger/tx/results/scp (+write in mirror-verify) | interleaved bundle | ✓ (decompressed) |
| `XdrParseLedger` | decode ledger frames + per-entry header hash | CPU (clean) | |
| `XdrParseTx` | decode tx frames + tx-set hash | CPU (clean) | |
| `XdrParseResult` | decode result frames + result-set hash | CPU (clean) | |
| `XdrParseScp` | decode/validate SCP frames | CPU (clean) | |
| `CrossFileVerify` | `verify_and_release` (completeness + hash cross-checks + intra-chain) | CPU (clean) | |
| `ChainVerify` | `verify_checkpoint_chain` (cross-cp boundary) | CPU (clean) | |
| `Copy` | plain `copy_from_reader` (mirror/repair **without** `--verify`) | IO read+write | ✓ (compressed) |

**Insertion points** (from the source map; place the guard at the start of each scope):
`verify.rs::verify_bucket_internal` → `BucketStream`; `xdr_verify.rs::decompress_and_write_internal` → `XdrDecompress`; the per-type parse loops `parse_{ledger_header,transaction,result}_entries_for_checkpoint` and the SCP parse → `XdrParse*`; `verify_and_release` → `CrossFileVerify`; `verify_checkpoint_chain` → `ChainVerify`; `pipeline.rs` history download/parse → `HistoryFetch`/`HistoryParse`; `storage.rs::copy_from_reader` → `Copy`.

### 2.2 Headline metrics

- **Wall time** of the operation (`Instant` around `cli::run`'s operation dispatch).
- **Peak RSS** via `getrusage(RUSAGE_SELF).ru_maxrss` at exit, normalized (macOS reports bytes; Linux reports KiB → ×1024).
- **Totals:** files processed, bytes processed → overall MB/s and files/s.

### 2.3 Output (when feature on)

On exit, print a phase table to stderr and append machine-readable rows:
- `perf-results/<run-id>/phases.csv`: `phase,total_ms,pct_of_phase_time,calls,mb_per_s`
- `perf-results/<run-id>/run.csv` (one row): `run_id,op,mode,concurrency,wall_ms,peak_rss_mb,files,bytes,mb_per_s,exit_code,…`
- A periodic sampler (background, feature-gated, every 2 s) appends `perf-results/<run-id>/timeseries.csv`: `t_s,files_done,bytes_done,peak_rss_mb_so_far` (for throughput-over-time on long runs).

### 2.4 Interpretation caveat (must appear in the final report)

Because checkpoints run concurrently and streaming uses spawned tasks, **phase times overlap and sum to more than wall-clock**. Treat the phase table as a *profiler-style self-time breakdown* ("where the work is"), not a wall-clock critical path. `BucketStream`/`XdrDecompress` are **bundles** (gzip + hash/IO interleaved) — they cannot be split further without per-chunk timing, which we deliberately avoid. Wall time and peak RSS are the authoritative headline numbers.

### 2.5 Build commands

```bash
# Clean, production-representative binary
cargo build --release
cp target/release/stellar-archivist ./bin/sa-clean

# Instrumented binary (separate copy so both exist)
cargo build --release --features perf-metrics
cp target/release/stellar-archivist ./bin/sa-perf
```
Use **`sa-clean`** for headline wall/RSS numbers (no instrumentation overhead) and **`sa-perf`** for the phase breakdown. Report both; if they diverge materially, note the instrumentation overhead.

---

## 3. Corruption tool spec (`corrupt-archive` bin)

A feature-gated `[[bin]]` (`--features perf-metrics`, or a dedicated `corruption-tool` feature) that reuses the existing test corruption helpers, promoted to a reachable module.

```
corrupt-archive <archive-dir> --kinds <list|all> [--count N] [--seed S] --manifest out.json
```

**Menu** (one per `--verify` rule, so the repair test exercises every check):

| Kind | Effect | Trips |
|---|---|---|
| `delete-file` | remove a random per-cp file or bucket | existence |
| `truncate` | truncate a file | gzip/parse |
| `byte-flip` | XOR bytes of a file | gzip/parse/bucket-hash |
| `invalid-gzip` | overwrite with non-gzip bytes | gzip |
| `bucket-hash` | valid gzip, wrong content | bucket SHA256 |
| `ledger-header-hash` | corrupt a header preserving frame validity | ledger-header hash |
| `drop-ledger` | remove one ledger entry from a checkpoint | completeness |
| `txset-hash` | corrupt `tx_set_hash` field, keep per-entry hash valid | tx-set cross-file |
| `result-hash` | corrupt result hash field, keep per-entry hash valid | result cross-file |
| `intra-chain` | break `prev_ledger_hash` within a checkpoint | intra-cp chain |
| `cross-chain` | break the boundary between adjacent checkpoints | cross-cp chain |
| `well-known` | delete/corrupt `.well-known/stellar-history.json` | well-known |

**Manifest** (`out.json`): exactly which files/checkpoints/buckets were broken and how — so the harness can assert the repair report's broken set matches, and that repair fixed precisely those.

---

## 4. Stage 0 — setup

1. `git switch -c perf-testing repair-v2` (work here; keep the PR clean).
2. Implement §2 and §3 on this branch; commit.
3. Build `sa-clean` and `sa-perf` (§2.5).
4. **Capture the environment** into `perf-results/env.txt`: OS + version, CPU model + **physical/logical core count**, RAM, disk type (SSD/NVMe), filesystem, rustc version, network (for remote runs: rough bandwidth via a one-off download). Run on **both macOS and Ubuntu** if comparing platforms; label every result with the host.
5. Sanity: `./bin/sa-clean scan <testnet-url> --high 1023` succeeds.

Testnet archive URL (confirmed in-repo): `https://history.stellar.org/prd/core-testnet/core_testnet_001`
Pubnet archive URL: SDF public archive, e.g. `https://history.stellar.org/prd/core-live/core_live_001` — **confirm the current canonical pubnet archive URL before the full run.**

---

## 5. Stage 1 — correctness / sanity (do this before perf; small + fast)

Use a **complete, self-contained** small archive as the source so a full-range mirror should be byte-identical. Recommended src: a local copy of `testdata/testnet-archive-small` (160 checkpoints, ~31 MB) — call it `$SRC` (a `file://` URL).

```bash
SRC=file://$PWD/testdata/testnet-archive-small
WORK=$(mktemp -d); MIR=$WORK/mirror; SNAP=$WORK/snapshot
```

### 5.1 Scan finds everything
```bash
./bin/sa-clean scan "$SRC" --verify --report "$WORK/scan.json"
```
**Pass:** exit 0; `scan.json` shows zero failures (`files`/`buckets`/`checkpoints` empty, `well_known` null); the success count equals the number of objects under the archive (cross-check: count files in `testdata/testnet-archive-small` excluding `.well-known`).

### 5.2 Mirror reproduces src
```bash
./bin/sa-clean mirror "$SRC" "file://$MIR" --verify --report "$WORK/mirror.json"
diff -r testdata/testnet-archive-small "$MIR" > "$WORK/mirror.diff" || true
```
**Pass:** exit 0; `mirror.diff` is **empty**. If the *only* difference is `.well-known/stellar-history.json`, that's acceptable **iff** `$MIR/.well-known/stellar-history.json` is byte-identical to the highest `history-*.json` in `$MIR` (the §1 rewrite rule) — assert that explicitly. Save the good mirror: `cp -r "$MIR" "$SNAP"`.

### 5.3 Repair restores a corrupted copy
```bash
./bin/corrupt-archive "$MIR" --kinds all --count 20 --seed 1 --manifest "$WORK/corrupt.json"
# (optional) confirm corruption is detected:
./bin/sa-clean scan "file://$MIR" --verify --report "$WORK/scan-corrupt.json" || true
./bin/sa-clean repair "$SRC" "file://$MIR" --verify --report "$WORK/repair.json"
diff -r "$SNAP" "$MIR" > "$WORK/repair.diff" || true
```
**Pass:** (a) `scan-corrupt.json` reports broken items matching `corrupt.json`; (b) repair exits 0; (c) `repair.diff` is **empty** (dst back to the pre-corruption snapshot), applying the same `.well-known` rule as 5.2; (d) `repair.json`'s broken set equals `corrupt.json`.

### 5.4 Repair dry-run produces a usable plan
```bash
./bin/corrupt-archive "$SNAP" --kinds all --count 10 --seed 2 --manifest "$WORK/c2.json"  # on a fresh copy
./bin/sa-clean repair "$SRC" "file://<copy>" --dry-run --verify --report "$WORK/plan.json"
./bin/sa-clean repair "$SRC" "file://<copy>" --plan "$WORK/plan.json"
```
**`--verify` on the dry-run is required:** without it the dry-run only detects *missing* files (existence), so content corruptions (hash/chain/bucket) are omitted from the plan and the subsequent `--plan` apply leaves them unrepaired. With `--verify` the plan captures all of them.

**Pass:** applying `--plan` restores the copy — `diff -r` against its pre-corruption snapshot is empty. (Note: a hard scan/verify failure may abort before the `--report` is written, so detection is confirmed via the non-zero exit code; the plan/report is produced by the dry-run, which completes.)

Run 5.1–5.4 once on v1 (`testnet-archive-small`) and once on **v2** (`testnet-archive-v2`, has `hotArchiveBuckets`) for format coverage.

---

## 6. Stage 2 — performance

### 6.0 Measurement methodology (read first)

- **Wall + peak RSS, authoritative:** wrap every run with OS `time`:
  - macOS: `/usr/bin/time -l <cmd>` → `maximum resident set size` (**bytes**).
  - Ubuntu: `/usr/bin/time -v <cmd>` (GNU time; `apt-get install time`) → `Maximum resident set size (kbytes)` and `Elapsed (wall clock) time`.
  The harness detects the OS and parses the right field. `sa-perf`'s in-process getrusage is a cross-check.
- **Time-series RSS (long runs):** the harness backgrounds `while kill -0 $PID; do ps -o rss= -p $PID; sleep 2; done` → `timeseries_rss.csv` (RSS in KB). Cross-platform, no code.
- **Repetitions:** run each configuration **3×**; report **median** (and min/max). Discard the first run if cold-cache effects dominate (note warm vs cold).
- **Isolation:** quiet machine, AC power (laptops), no other heavy IO. For local-source tests, pre-warm the OS file cache once (or explicitly test cold vs warm and label it).
- **No `--debug/--trace`** during perf runs.
- **Disk: use the dedicated data volume, not the root/OS disk.** All large
  fixtures, mirror destinations, repair copies, and harness temp dirs must live
  on the big scratch volume (on the Ubuntu/Graviton host: the 14 TB RAID0 NVMe
  mounted at `/data`, e.g. `/data/perf/...`). The root disk (~90 GB EBS) cannot
  hold a 55 GB fixture, let alone the per-run copies the sweep makes, and EBS is
  far slower than the instance-store NVMe — measuring on it would both run out
  of space and distort IO numbers. Point `TMPDIR`/`mktemp` at the data volume
  too (the default `/tmp` is on root).

### 6.1 Scaling test — time/RSS vs `-c` (local source, removes network noise)

**Fixture:** a substantial **local** archive so concurrency has work to show. Create once by mirroring a bounded pubnet range locally (e.g. ~2,000 checkpoints), then use it read-only as `file://`:
```bash
# Use a RECENT 2,000-checkpoint window, ending at the current tip — NOT the
# first 2,000 checkpoints from genesis.
./bin/sa-clean mirror "$PUBNET" "file:///data/perf/fixture" --low <L> --high <H>
FIX=file:///data/perf/fixture
```
> **Use the recent ~2,000 checkpoints, not the first ~2,000.** Two reasons:
> (1) **Genesis pubnet predates SCP archival** — `scp-*.xdr.gz` files 404 for the
> early range, so a plain mirror fails (exit 2) unless you pass `--skip-optional`,
> and the fixture is then missing a whole file type the verify path should exercise.
> (2) Early ledgers are nearly empty, so a genesis window is tiny (~50 MB) and
> **won't stress concurrency** — the whole point of the scaling test. A recent
> window has all five file types and realistic per-checkpoint sizes (≈55 GB for
> 2,000 cp), which actually exercises `-c`. Pick the range from the live tip:
> `high = (currentLedger+1)/64*64 - 1`, `low = high - 2000*64 + 1`. Lives on
> `/data` per the disk note in §6.0.

**Sweep** `-c ∈ {1,2,4,8,16,32,64}` (extend to 96/128 if not yet plateaued) across these **four** modes (a 2×2 of operation × verify):
- `scan` (existence only), `scan --verify`
- `mirror` (to a fresh empty dst each run), `mirror --verify`

> **Repair is intentionally NOT in the scaling sweep.** The 2×2 above isolates
> the two axes a concurrency curve cares about: existence-vs-content-verify (the
> decompress+hash CPU cost) and read-only-vs-read+write (scan vs mirror IO).
> Repair adds neither a new axis nor a clean signal here: run *without* `--verify`
> it detects only missing files, so its curve tracks plain scan; run *with*
> `--verify` it tracks scan-verify plus a re-fetch tail. It also needs a fresh
> 55 GB corrupted copy per cell, which dominates the cell with copy/corruption
> setup rather than the thing we're measuring. Repair **correctness** is fully
> covered in Stage 1 (§5, all 12 kinds), and repair **performance** is measured
> in the full-pubnet stage (§6.2). Keeping it out of §6.1 makes the scaling
> result clean and roughly halves the sweep's wall time.

For each (mode × `-c` × rep): record wall, peak RSS, throughput, exit code → `run.csv`; and one instrumented (`sa-perf`) run per (mode × `-c`) for `phases.csv`. Run **3 reps** per cell (plan §6.0) and report the median plus min/max.

**Plots** (`scripts/perf/plot.py`, matplotlib → PNG):
- wall-time vs `-c`, one line per mode (find the knee / plateau).
- peak-RSS vs `-c`, one line per mode.
- stacked **phase breakdown** bar per mode at the plateau `-c`.

### 6.2 Full pubnet (true full, `-c=32`, remote)

**⚠️ Large:** hundreds of GB of disk, many hours. Ensure disk headroom; run in `tmux`/`nohup`. Mirror is resumable (re-running continues); the harness logs progress and can resume.

Order (mirror first so scan/repair can optionally use the local copy too):
All paths on the dedicated data volume (`/data`), never the root disk — see the disk note in §6.0.
```bash
DST=file:///data/pubnet-mirror
# 1) MIRROR (downloads everything)
time-wrap ./bin/sa-clean mirror "$PUBNET" "$DST" -c 32 --report mirror.json
# 2) SCAN remote (existence) and SCAN remote --verify
time-wrap ./bin/sa-clean scan "$PUBNET" -c 32 --report scan.json
time-wrap ./bin/sa-clean scan "$PUBNET" -c 32 --verify --report scan-verify.json
# 3) REPAIR: corrupt a copy of the local mirror, repair from remote
./bin/corrupt-archive /data/pubnet-copy --kinds all --count <K> --manifest c.json
time-wrap ./bin/sa-clean repair "$PUBNET" file:///data/pubnet-copy -c 32 --verify --report repair.json
time-wrap ./bin/sa-clean repair "$PUBNET" file:///data/pubnet-copy -c 32 --dry-run --report plan.json
```
Capture for each: wall, peak RSS, throughput (MB/s, files/s), the phase breakdown (one `sa-perf` run; for the very longest, `sa-perf` mirror may be skipped if overhead is a concern — note it), and the `ps` RSS time-series. **Remote runs are network-bound** — explicitly note measured bandwidth so CPU/IO bottlenecks aren't confused with network limits. Re-run the scan/repair against the **local** full mirror (`file://`) to get the network-free engine numbers for comparison.

---

## 7. Results layout & schema

```
perf-results/
  env.txt
  fixture/                     # local scaling fixture (gitignored)
  <run-id>/                    # one dir per run: run-id = <host>_<op>_<mode>_C<n>_<rep>
    cmd.txt  stdout.log  stderr.log
    run.csv  phases.csv  timeseries.csv  timeseries_rss.csv
    report.json                # the archivist --report output
  summary.csv                  # all run.csv rows concatenated
  plots/*.png
```
`run.csv` columns: `run_id,host,binary(clean|perf),op,mode,concurrency,rep,wall_ms,peak_rss_mb,files,bytes,mb_per_s,files_per_s,exit_code`.

---

## 8. Final deliverable — performance report

`docs/perf-report.md` containing: environment table; **headline table** (op × mode → wall, peak RSS, throughput, at default `-c=32`); **scaling plots** + the knee/plateau finding per mode; **phase-breakdown** plots + narrative ("X% of work is bucket gzip+hash", etc., with the §2.4 caveat stated); **full-pubnet** results (remote vs local-engine, network-bound note); **correctness** confirmation (Stage 1 all green); and **observations/bottlenecks** + any recommendations.

---

## 9. Cheat sheet

```bash
# build both binaries
cargo build --release && cp target/release/stellar-archivist bin/sa-clean
cargo build --release --features perf-metrics && cp target/release/stellar-archivist bin/sa-perf
# one measured run (harness wraps OS time + ps sampler)
scripts/perf/run.sh <run-id> sa-clean scan "$SRC" --verify -c 32
# scaling sweep + plots
scripts/perf/scaling.sh "$FIX" && scripts/perf/plot.py perf-results/summary.csv
```
```
