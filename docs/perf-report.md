# Stellar Archivist — Performance Report (Stage 1 + Stage 2.1)

**Scope:** local execution on **testnet** archives only, through plan §6.1 (scaling).
Full-pubnet (§6.2) was intentionally **not** run. Raw artifacts are preserved under
`perf-results/` (gitignored, on disk): per-run dirs, `summary.csv`, `plots/`, `scaling.log`,
and the Stage-1 outputs under `perf-results/stage1/{v1,v2}/`.

## Environment

| | |
|---|---|
| Host | Apple M1 Pro, **10 cores** (10 physical/logical) |
| RAM | 32 GiB |
| OS | macOS (Darwin 25.4.0, arm64) |
| rustc | 1.94.1 |
| Binaries | `bin/sa-clean` (release), `bin/sa-perf` (release + `perf-metrics`), `bin/corrupt-archive` |
| Fixtures | `testnet-archive-small` (v1, 160 cp, 1954 files, 31 MB), `testnet-archive-v2` (v2, 250 cp, 3117 files, 54 MB) |

Wall time + peak RSS come from the in-process instrumentation (validated to match
`/usr/bin/time -l` on macOS); `peak_rss_mb_os` is the OS cross-check.

## Stage 1 — Correctness (all PASS, v1 and v2)

| Check | v1 (small) | v2 (hotArchiveBuckets) |
|---|---|---|
| §5.1 scan `--verify` clean; inventory match | ✅ succeeded **1954 == 1954** files | ✅ **3117 == 3117** |
| §5.2 mirror `--verify` → `diff -r` vs src | ✅ **empty** (byte-identical incl. `.well-known`) | ✅ **empty** |
| §5.3 corrupt(20) → scan detects (exit 2) → repair → `diff -r` vs snapshot | ✅ **empty** | ✅ **empty** (20 corruptions, 10 kinds) |
| §5.4 corrupt(10) → dry-run `--verify` plan → apply `--plan` → `diff -r` | ✅ **empty** | ✅ **empty** |

Notes: `--verify` is required on the repair dry-run for the plan to capture *content*
corruptions (existence-only dry-run misses hash/chain/bucket damage). A hard scan/verify
failure aborts before `--report` is written, so corruption detection is confirmed via the
non-zero exit code.

## Stage 2.1 — Concurrency scaling (fixture: testnet-archive-v2, 250 cp)

Sweep: `-c ∈ {1,2,4,8,16,32,64}` × {scan, scan-verify, mirror, mirror-verify, repair-dry-run, repair} × 2 reps (84 runs, all exit 0). Values below are the **min across reps**.

### Wall time (ms) vs `-c`

| mode | c=1 | c=2 | c=4 | c=8 | c=16 | c=32 | c=64 |
|---|---|---|---|---|---|---|---|
| scan            | 105   | 66    | 59    | 57    | 55    | 58    | 65   |
| scan-verify     | 709   | 440   | 450   | 424   | 485   | 459   | 441  |
| mirror          | 2614  | 2436  | 2501  | 2712  | 2706  | 2758  | 2772 |
| mirror-verify   | 16573 | 16184 | 16805 | 16614 | 16329 | 16513 | 16479|
| repair-dry-run  | 83    | 75    | 64    | 65    | 70    | 74    | 74   |
| repair          | 1419  | 1394  | 1414  | 1434  | 1404  | 1436  | 1483 |

### Peak RSS (MB) vs `-c`

| mode | c=1 | c=2 | c=4 | c=8 | c=16 | c=32 | c=64 |
|---|---|---|---|---|---|---|---|
| scan          | 35.0 | 35.5 | 36.1 | 37.5 | 37.9 | 42.0 | 44.0 |
| scan-verify   | 84.4 | 105.1| 156.4| 165.3| 161.0| 175.5| 207.3|
| mirror        | 47.4 | 51.5 | 66.4 | 89.4 | 93.6 | 96.7 | 109.8|
| mirror-verify | 58.1 | 81.1 | 135.6| 85.3 | 76.2 | 83.2 | 110.7|
| repair        | 33.9 | 33.8 | 34.7 | 37.8 | 37.0 | 41.6 | 44.5 |

Plots: `perf-results/plots/time_vs_concurrency.png`, `rss_vs_concurrency.png`.

### Key findings

1. **Concurrency plateaus very early on local data.** scan-verify gains ~1.6× from c=1→2 then is flat through c=64; plain scan ~2× by c=8 then flat. The knee is ≈ **c=2–4**, far below the 10 cores. Likely because each checkpoint already parallelizes its files internally (join_all + spawned hash tasks), so a few in-flight checkpoints already saturate the cores.
2. **`mirror` / `mirror-verify` are flat across all `-c`** → **I/O-bound** (writing 3117 files to local disk), not concurrency-bound. Adding checkpoint concurrency does not speed up local writes.
3. **`-c`'s real value is for remote archives** (network-latency hiding), which §6.2 (not run here) would exercise. The default `-c=32` is sensible for remote; for local processing `-c≈4` is enough.
4. **Peak RSS grows with `-c`** roughly linearly (more in-flight checkpoints = more buffered data): scan-verify 84→207 MB, mirror 47→110 MB from c=1→64. A concurrency/memory trade-off.
5. **Verify cost = decompression + hashing.** Phase share at the plateau (scan-verify, c=8): `bucket_stream` **58%** + `xdr_decompress` **33%** ≈ **91%** in gzip-decompress + SHA256; XDR decode/parse and cross-file/chain verification are <2% combined. Plain `mirror` is **99% `copy`** (file I/O). `mirror-verify` is **76% `bucket_stream`** (which bundles decompress+hash+write).

### Measurement caveat (important)

Phase times are **aggregate self-time summed across concurrent tasks and spawned-task awaits**, so they overlap and sum to **far more than wall-clock** (e.g. scan-verify c=8: ~21.7 s aggregate vs 0.42 s wall). Read the phase **percentages** as "where the work is," not the absolute ms as durations. `bucket_stream`/`xdr_decompress` also bundle gzip+hash(+write) and cannot be split further without per-chunk timing (deliberately avoided to stay lightweight). Headline **wall time** and **peak RSS** are the authoritative absolute numbers. Per-phase MB/s is understated for the same reason and should be ignored; the `copy` phase reports 0 bytes (plain-copy byte count not tracked).

## Issues found & fixed during testing

- **corrupt-archive panicked** on multi-corruption runs when a ledger-mutating kind re-selected an already-corrupted file (`read_and_parse_ledger_file` `.expect` on unparseable gzip). Fixed by making the readers fallible (skip the attempt). 0 panics across 16 stress runs after. (commit `4e6a39d`)
- **Doc §5.4** required `--verify` on the repair dry-run (else the plan misses content corruptions). Fixed. (commit `1837e9a`)
- **Concurrency flag is `-c`** (lowercase), not `-C`; docs corrected. (commit `770ee25`)

## Not run (out of scope here)

- §6.2 full-pubnet (true full) — large/remote; would surface network-bound throughput and RSS-over-time where high `-c` matters.
