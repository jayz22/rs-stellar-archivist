# Perf Harness

Scripts for measuring `stellar-archivist` throughput and memory scaling.

## Prerequisites

Build the three binaries and place them under `bin/`:

```bash
cargo build --release
cp target/release/stellar-archivist bin/sa-clean

cargo build --release --features perf-metrics
cp target/release/stellar-archivist bin/sa-perf

cargo build --release --features cli,corruption-tool
cp target/release/corrupt-archive bin/corrupt-archive
```

`bin/sa-perf` writes `phases.csv`, `headline.csv`, and `timeseries.csv` to
`$SA_PERF_OUT` (set automatically by `run.sh`), and emits a
`PERF wall_ms=... peak_rss_mb=...` line to stderr.

See `docs/perf-testing-plan.md` for methodology and acceptance criteria.

## Usage

```bash
# Record host environment
scripts/perf/env.sh

# Concurrency sweep across all modes (scan, mirror, repair, …)
scripts/perf/scaling.sh file://<fixture> bin/sa-perf bin/corrupt-archive

# Plot results
scripts/perf/plot.py
```

`SA_CS` overrides the concurrency levels swept (default `"1 2 4 8 16 32 64"`).
`SA_REPS` overrides the number of repetitions per cell (default `2`).
Results land in `perf-results/` (gitignored).
