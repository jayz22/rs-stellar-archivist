#!/usr/bin/env bash
# phase1_sweep.sh — run the scan-verify subset sweep for each gzip-backend
# variant, sequentially (no inter-variant contention). 1 rep, probe at c=16.
set -o pipefail
export PATH="$HOME/.cargo/bin:$PATH"
export TMPDIR=/data/perf/tmp; mkdir -p "$TMPDIR"
cd "$(dirname "$0")/../.."

run_variant() { # <label> <bin>
  echo "##### $(date -u +%H:%M:%S) variant=$1 bin=$2 #####"
  SA_CS="1 4 16" SA_REPS=1 scripts/perf/verify_subsweep.sh "verifyperf/$1" "$2" 16
}

run_variant zlib-rs bin/sa-perf-zrs
run_variant cloudflare bin/sa-perf-cf
run_variant zlib-ng bin/sa-perf-zng
echo "##### PHASE1 SWEEP DONE $(date -u +%H:%M:%S) #####"
