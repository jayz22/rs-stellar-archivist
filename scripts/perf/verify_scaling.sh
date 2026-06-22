#!/usr/bin/env bash
# verify_scaling.sh — sweep -c (checkpoint/decode parallelism) at fixed high
# --max-concurrent, in scan --verify mode, to test whether verify decode scales
# across cores or flattens. Companion to docs/verify-scaling-investigation.md.
#
# Unlike maxconc_sweep.sh (sweeps mc), this sweeps -c: for verify, -c bounds how
# many buckets decode concurrently, so it's the knob that should drive core use.
# mc is held HIGH so downloads never gate decode. Intended for the LOCAL-SIM
# archive (loopback) so network is effectively infinite and any flat curve with
# idle cores is a DESIGN bottleneck, not bandwidth/latency.
#
# Usage: verify_scaling.sh <label> <binary> <archive-url>
# Env:
#   SA_CS    list of -c values (default "1 4 8 16 32")
#   SA_MC    fixed --max-concurrent (default 128)
#   SA_LOW SA_HIGH  ledger bounds (define small vs large range)
#   SA_NIC   interface for bottleneck.sh net BW (default lo, for local-sim)
set -o pipefail
export PATH="$HOME/.cargo/bin:$PATH"
LABEL="$1"; BIN="$2"; ARCHIVE="$3"
[ -n "$ARCHIVE" ] || { echo "usage: verify_scaling.sh <label> <binary> <archive>"; exit 2; }
CS="${SA_CS:-1 4 8 16 32}"
MC="${SA_MC:-128}"
LOW="${SA_LOW:-63045000}"; HIGH="${SA_HIGH:-63046015}"
export SA_NIC="${SA_NIC:-lo}"
# Local-sim mirror was built with --skip-optional (no SCP files), so verify must
# skip them too or every checkpoint reports a missing scp file. Default on.
SKIP_OPT=(); [ "${SA_SKIP_OPTIONAL:-1}" = "1" ] && SKIP_OPT=(--skip-optional)

echo "=== verify scaling [$LABEL] -c={$CS} mc=$MC range=$LOW..$HIGH NIC=$SA_NIC skip_opt=${SA_SKIP_OPTIONAL:-1} ==="
for C in $CS; do
  RID="${LABEL}/verify_c${C}"
  echo "--- cell: -c=$C ---"
  ( scripts/perf/run.sh "$RID" "$BIN" scan "$ARCHIVE" \
      -c "$C" --max-concurrent "$MC" --verify "${SKIP_OPT[@]}" --low "$LOW" --high "$HIGH" ) &
  RUN_PID=$!
  NH="perf-results/${RID}/nethealth.txt"; BN="perf-results/${RID}/bottleneck.txt"
  mkdir -p "perf-results/${RID}"; : > "$NH"; : > "$BN"
  sleep 3
  PID=""
  for p in $(pgrep -f 'sa-perf|sa-verify|stellar-archivist'); do
    case "$(ps -o comm= -p "$p" 2>/dev/null | xargs)" in bash|sh|zsh|dash|time|nohup|pgrep) continue;; esac
    PID="$p"; break
  done
  # Sample THROUGHOUT the run (not just the first few s) so we see sustained core
  # utilization + loopback throughput, not just the ramp. Cap is a safety stop.
  s=0
  while kill -0 "$RUN_PID" 2>/dev/null && [ "$s" -lt 400 ]; do
    if [ -n "$PID" ] && [ -d "/proc/$PID" ]; then
      echo "--- nethealth sample $((s+1)) (c=$C) ---" >> "$NH"
      scripts/perf/nethealth.sh "$PID" 5 >> "$NH" 2>&1
    fi
    echo "--- bottleneck sample $((s+1)) (c=$C) ---" >> "$BN"
    scripts/perf/bottleneck.sh 'sa-verify|sa-perf|stellar-archivist' 5 >> "$BN" 2>&1
    s=$((s+1))
  done
  wait "$RUN_PID"
  REP="perf-results/${RID}/report.json"
  if [ -f "$REP" ]; then
    read -r OK FAIL RETR < <(python3 - "$REP" <<'PY'
import json,sys
d=json.load(open(sys.argv[1]))["summary"]
print(d.get("succeeded",0), d.get("failed",0), d.get("retries",0))
PY
)
    W=$(awk -F, 'NR==2{print $1}' "perf-results/${RID}/headline.csv" 2>/dev/null)
    MBPS=$(awk -F, 'NR==2{print $5}' "perf-results/${RID}/headline.csv" 2>/dev/null)
    echo "  [c=$C] wall_ms=$W mb_per_s=$MBPS succeeded=$OK failed=$FAIL retries=$RETR"
    [ "${FAIL:-0}" -gt 0 ] && { echo "  !! failed=$FAIL — correctness gate broken, stopping"; break; }
  fi
done
echo "verify scaling [$LABEL] done"
