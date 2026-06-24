# Perf Harness

Scripts for measuring `stellar-archivist` throughput, memory, phase timing, and
core-scaling. All instrumentation is **feature-gated and off by default** —
production builds are unaffected.

## Prerequisites

Build the binaries the scripts expect under `bin/` (gitignored):

```bash
# production binary (no instrumentation) — used as the correctness baseline
cargo build --release
cp target/release/stellar-archivist bin/sa-clean

# instrumented binary — emits `PERF`, `PERF_CPU`, `PERF_HEARTBEAT`, and `PERF_PHASE` lines to stderr
cargo build --release --features perf-metrics
cp target/release/stellar-archivist bin/sa-perf
```

`bin/sa-perf` writes perf metrics to stderr. `run.sh` captures that stream in
each run directory and appends the parsed headline values to
`perf-results/summary.csv`.

`PERF_HEARTBEAT` reports runtime responsiveness: a 10 ms heartbeat task's
wake-lag (`beats`, `mean_us`, `max_us`, and `over_1ms`/`over_10ms`/`over_100ms`
counts). A growing `over_10ms`/`over_100ms` tail means the worker pool is too
busy to promptly poll a ready task. Counts only — derive ratios (e.g.
`over_10ms`/`beats`) in scripts.
`run.sh` carries `max_us` and `over_10ms` into `summary.csv` as
`heartbeat_max_us` and `heartbeat_over_10ms` for sweep analysis.

For Tokio-worker runtime diagnostics (`RTM busy_cores=…`, recorded by
`scaling_graph.sh` as secondary worker-pool columns), build the perf binary
with `RUSTFLAGS="--cfg tokio_unstable"`. The main scaling graph uses
process-wide `cores_avg` from `PERF_CPU`, so it remains valid without RTM lines.
No extra runtime environment variable is needed.

Python scripts (`plot*.py`, `analyze.py`) need `python3` + `matplotlib`.
Results land in `perf-results/` (gitignored).

## Scripts

### Core building blocks
| Script | Purpose |
|---|---|
| `lib.sh` | Sourced helpers shared by the rest: `resolve_pid`, `perf_fields`, `report_summary`. Not run directly. |
| `env.sh [out-dir]` | Record host environment (CPU/cores/RAM/OS/rustc) → `env.txt` |
| `run.sh <run-id> <binary> <op> [args…]` | Run once under `/usr/bin/time`, capture stderr perf lines + OS peak-RSS cross-check, auto-inject `--report`, append `summary.csv` |

### Sweeps
| Script | Purpose |
|---|---|
| `scaling.sh <fixture-url> [binary]` | Concurrency (`-c`) sweep across scan, scan-verify, mirror, mirror-verify. Env: `SA_CS`, `SA_REPS` (default 3), `SA_LOW`/`SA_HIGH`, `SA_TMP` |
| `verify_scaling.sh <label> <binary> <archive-url>` | Sweep `-c` in verify mode at fixed-high `--max-concurrent`, sampling the `bottleneck.sh` limiter mid-run. Env: `SA_CS`, `SA_MC`, `SA_LOW`/`SA_HIGH`, `SA_NIC` |
| `scaling_graph.sh <label> <binary> "<core-list>"` | `taskset` core-affinity cap sweep → throughput vs N cores (scaling graph). Uses process-wide `cores_avg` for utilization and keeps Tokio worker busy cores as secondary diagnostics. Env: `SA_C`, `SA_MC`, `SA_LOW`/`SA_HIGH`, `SA_ARCHIVE` |

### Live diagnostics (sample a running process)
| Script | Purpose |
|---|---|
| `bottleneck.sh [pid-or-pattern] [window_s]` | Sample **all** dimensions (CPU/net/conns/disk-BW/IOPS/await/threads) and name the limiter. Env: `SA_NIC`, `SA_DISKS` |

### Analysis / plotting
| Script | Purpose |
|---|---|
| `plot.py [summary.csv]` | Wall-time + peak-RSS vs `-c`, one line per mode |
| `analyze.py [perf-results] [plateau_c]` | Median (min–max) Markdown tables + stacked phase-breakdown bar from stderr logs |
| `plot_scaling_graph.py [scaling_graph.csv]` | Throughput-vs-cores + core-utilization panels |

## Examples

```bash
# host env + concurrency sweep + plots
scripts/perf/env.sh
SA_LOW=63 SA_HIGH=15999 scripts/perf/scaling.sh file://$PWD/testdata/testnet-archive-v2 bin/sa-perf
scripts/perf/plot.py

```
