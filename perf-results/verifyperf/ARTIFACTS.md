# verify-speedup experiment — artifact index

All intermediate artifacts for the verify CPU-scaling experiment
(`docs/perf-verify-speedup-plan.md`, branch `perf-verify-speedup`) live here and
are **kept for explanation + reproducibility**. Nothing is deleted.

> Note: `perf-results/` is gitignored (raw artifacts), per the original perf
> plan. These files are preserved **on disk in the worktree**, not in git. The
> *summaries/conclusions* go into the committed `docs/` reports.

## Layout

```
perf-results/verifyperf/
  ARTIFACTS.md                  # this index
  summary-phase1.csv            # snapshot of the shared run.csv rows for Phase 0+1
  env/                          # reproducibility: what was built & how
    backends.txt                # cargo-tree proof of which gzip lib each binary links
    binaries.txt                # bin/sa-perf* sizes
    build-sa-perf-baseline.log  # build log: sa-perf (miniz baseline)
    build-variants.log          # build log: zlib-rs / cloudflare / zlib-ng variants
  smoke/                        # Phase 1 correctness gate (small 501-cp range)
    smoke-run.log               # per-variant timing + exit/summary
    smoke-miniz-baseline.json   # report.json per backend (all: failed=0, 6492 ok)
    smoke-zlib-rs.json
    smoke-cloudflare.json
    smoke-zlib-ng.json
  logs/                         # sweep driver stdout
    baseline-sweep-c1-aborted2rep.log  # initial 2-rep run, aborted -> switched to 1 rep
    baseline-sweep-c4-c16.log
    zlib-ng-sweep.log
  baseline/                     # Phase 0 baseline (miniz), per cell
    probe_C16.txt               # CPU-utilization probe during c=16 (the "before")
    scanverify_C{1,4,16}_r1/     # cmd.txt stdout.log stderr.log report.json
                                #   headline.csv phases.csv timeseries.csv
  zlib-ng/                      # Phase 1 zlib-ng, per cell (same per-run files)
    probe_C16.txt
    scanverify_C{1,4,16}_r1/
```

Each per-run dir (written by `scripts/perf/run.sh`) contains:
`cmd.txt` (exact command), `stdout.log`/`stderr.log`, `report.json` (archivist
`--report`: success/failed/retries + broken set), `headline.csv`
(wall_ms,peak_rss_mb,files,bytes,mb_per_s from in-process `sa-perf`),
`phases.csv` (self-time phase breakdown), `timeseries.csv` (2 s RSS samples).

## Experiment → artifact map

| Phase | What | Where |
|---|---|---|
| 0 | Baseline calibration (c=1/4/16) | `baseline/scanverify_C*` |
| 0 | "Before" CPU probe (~0.6 cores @ c=16) | `baseline/probe_C16.txt` |
| 1 | Backend builds + which lib links | `env/build-variants.log`, `env/backends.txt` |
| 1 | Correctness gate (all backends, failed=0) | `smoke/` |
| 1 | zlib-ng sweep (c=1/4/16) | `zlib-ng/scanverify_C*` |
| 1 | zlib-ng CPU probe @ c=16 | `zlib-ng/probe_C16.txt` |
| 0+1 | Aggregated headline rows | `summary-phase1.csv` (and `perf-results/summary.csv`) |

## Reproduce

```bash
export PATH="$HOME/.cargo/bin:$PATH"
# 1. binaries (baseline + 3 backends), all with perf-metrics
cargo build --release --features perf-metrics              && cp target/release/stellar-archivist bin/sa-perf
cargo build --release --features perf-metrics,fast-zlib-rs && cp target/release/stellar-archivist bin/sa-perf-zrs
cargo build --release --features perf-metrics,fast-zlib-cf && cp target/release/stellar-archivist bin/sa-perf-cf
cargo build --release --features perf-metrics,fast-zlib-ng && cp target/release/stellar-archivist bin/sa-perf-zng
# 2. warm the fixture, then sweep (1 rep, probe at c=16)
SA_CS="1 4 16" SA_REPS=1 scripts/perf/verify_subsweep.sh verifyperf/<label> bin/<binary> 16
```

## Artifact-handling policy (this experiment, going forward)

1. **Keep everything.** No intermediate result is deleted; if a step produces
   output, it lands under `perf-results/verifyperf/<phase-or-mode>/`.
2. **One dir per run.** Always give each measured run its own `SA_PERF_OUT` dir
   (via `run.sh`). **Never** set `SA_PERF_OUT=""` — that dumps metrics to the CWD
   and overwrites across runs. (Early smoke runs hit this; the smoke `report.json`s
   are preserved in `smoke/` but their in-process `headline/phases/timeseries`
   were overwritten and not kept — the only gap, and immaterial since smoke was a
   correctness check, not a timing one.)
3. **Copy ephemeral logs in.** Background-task stdout lives under `/tmp/.../tasks/`
   and is session-scoped; copy anything worth keeping into `logs/` or `env/`.
4. **Snapshot the shared `summary.csv`** per phase (`summary-phaseN.csv`) since
   `run.sh` appends all runs to one shared file.
