# `--max-concurrent` experiment — artifact index (Phase A)

All artifacts for the `--max-concurrent` experiment live here and are **kept for
explanation + reproducibility** (force-added past the `perf-results/` gitignore, as
small text files, same policy as `perf-results/verifyperf/`). Analysis &
conclusions: `docs/max-concurrent-progress-report.md`. Plan:
`docs/max-concurrent-sweep-plan.md`.

> Target: public `history.stellar.org/prd/core-live/core_live_001` (HTTPS).
> Host `user-dev-007` (Graviton2, 32 vCPU), quiet. Binary `bin/sa-perf`
> (`--features perf-metrics`). 1 rep/cell.

## Layout

```
perf-results/maxconc/
  ARTIFACTS.md                  # this index
  smoke_exist/                  # connectivity smoke: existence-scan, ~16 cp
  smoke_verify/                 # connectivity smoke: scan-verify, ~2 cp (bytes/cost probe)
  exist/                        # existence sweep, 2000 cp, -c=512, grid mc 8..512 (+curve.csv)
  exist_big/                    # MAIN existence sweep, 10000 cp, -c=256, grid mc 8..256
    exist_mc{8,16,32,64,128,256}/   # per-cell run dir + nethealth.txt + bottleneck.txt
    curve.csv                   # throughput + core/thread/health summary per mc
  c_control/                    # -c control: fixed mc, vary -c (proves -c is futures not cores)
    mc128_c{32,128,512}/  mc64_c{32,64,512}/
    curve.csv
```

Each per-run dir (from `scripts/perf/run.sh`): `cmd.txt`, `stdout.log`,
`stderr.log`, `report.json` (`--report`: succeeded/failed/retries), `headline.csv`
(wall_ms,peak_rss_mb,files,bytes,mb_per_s), plus for sweep cells `nethealth.txt`
(cores/threads/conns/RTT/TCP-health samples) and `bottleneck.txt` (limiter verdict).

## Instruments
- `scripts/perf/maxconc_sweep.sh` — sweep driver (fixed `-c`, ascending `mc` ramp,
  per-cell sampling, retry/fail hard-stop).
- `scripts/perf/nethealth.sh` — per-window evidence sampler (cores, thread state +
  wchan, `:443` conns, median SRTT + minRTT, TCP retrans/timeouts/fails). No root.
- `scripts/perf/bottleneck.sh` — named limiter verdict.

## Reproduce
```bash
export PATH="$HOME/.cargo/bin:$PATH"
ARCHIVE="https://history.stellar.org/prd/core-live/core_live_001"
# existence main sweep:
SA_C=256 SA_MCS="8 16 32 64 128 256" SA_LOW=62406015 SA_HIGH=63046015 \
  scripts/perf/maxconc_sweep.sh maxconc/exist_big bin/sa-perf "$ARCHIVE" exist
```

## Headline result
Existence-scan knee at **`--max-concurrent` ≈ 128** (default 64 ≈ 1.7× too low);
network-latency-bound (<2 of 32 cores); **0 retrans / 0 timeouts / 0 fails** through
280 connections. Full table + evidence in the progress report.
