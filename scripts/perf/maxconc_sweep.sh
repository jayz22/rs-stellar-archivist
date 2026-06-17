#!/usr/bin/env bash
# maxconc_sweep.sh — sweep --max-concurrent against a REMOTE archive to find the
# throughput knee. Companion to docs/max-concurrent-sweep-plan.md.
#
# Isolates --max-concurrent as the sole variable: -c (checkpoint parallelism) is
# held FIXED and HIGH so it is never the limiter (plan §3); only --max-concurrent
# (the per-backend I/O semaphore = effective HTTP connection cap) varies.
#
# Usage: maxconc_sweep.sh <label> <binary> <archive-url> <mode>
#   label   : subdir under perf-results/ (e.g. maxconc/exist)
#   binary  : path to sa-perf-style binary (--features perf-metrics)
#   archive : remote archive URL (http/https) — file:// makes the knob inert
#   mode    : exist | verify   (verify adds --verify: download+decompress+sha256)
#
# Env:
#   SA_MCS        list of --max-concurrent values, ascending (default "8 16 32 64 128 256")
#   SA_C          fixed -c / --concurrency (default 64, must be >= max MC tested)
#   SA_LOW SA_HIGH  ledger bounds
#   SA_BOTTLENECK 1 = sample bottleneck.sh mid-run (default 1)
#   SA_RETRY_STOP integer; abort the sweep if a cell's report retries exceed this
#                 (backend-health hard stop, plan §7; default 10)
set -o pipefail
export PATH="$HOME/.cargo/bin:$PATH"
LABEL="$1"; BIN="$2"; ARCHIVE="$3"; MODE="$4"
[ -n "$MODE" ] || { echo "usage: maxconc_sweep.sh <label> <binary> <archive> <exist|verify>"; exit 2; }
MCS="${SA_MCS:-8 16 32 64 128 256}"
C="${SA_C:-64}"
LOW="${SA_LOW:-63040000}"; HIGH="${SA_HIGH:-63046015}"
DO_BN="${SA_BOTTLENECK:-1}"
RETRY_STOP="${SA_RETRY_STOP:-10}"

VERIFY_FLAG=()
[ "$MODE" = "verify" ] && VERIFY_FLAG=(--verify)

echo "=== maxconc sweep [$LABEL] mode=$MODE -c=$C MCs={$MCS} range=$LOW..$HIGH ==="
echo "    archive=$ARCHIVE  retry-stop=$RETRY_STOP"

for MC in $MCS; do
  RID="${LABEL}/${MODE}_mc${MC}"
  echo "--- cell: --max-concurrent=$MC ---"
  # Launch in background so we can sample bottleneck.sh mid-flight.
  ( scripts/perf/run.sh "$RID" "$BIN" scan "$ARCHIVE" \
      -c "$C" --max-concurrent "$MC" "${VERIFY_FLAG[@]}" \
      --low "$LOW" --high "$HIGH" ) &
  RUN_PID=$!

  if [ "$DO_BN" = "1" ]; then
    BN_LOG="perf-results/${RID}/bottleneck.txt"
    NH_LOG="perf-results/${RID}/nethealth.txt"
    mkdir -p "perf-results/${RID}"; : > "$BN_LOG"; : > "$NH_LOG"
    sleep 3
    # Resolve the REAL binary pid (skip the bash/zsh/time wrappers), like bottleneck.sh.
    PID=""
    for p in $(pgrep -f 'sa-perf|stellar-archivist'); do
      case "$(ps -o comm= -p "$p" 2>/dev/null | xargs)" in bash|sh|zsh|dash|time|nohup|pgrep) continue;; esac
      PID="$p"; break
    done
    s=0
    while kill -0 "$RUN_PID" 2>/dev/null && [ "$s" -lt 8 ]; do
      if [ -n "$PID" ] && [ -d "/proc/$PID" ]; then
        echo "--- nethealth sample $((s+1)) (mc=$MC) ---" >> "$NH_LOG"
        scripts/perf/nethealth.sh "$PID" 5 >> "$NH_LOG" 2>&1
      fi
      # One bottleneck verdict per cell (for the net-BW + named limiter).
      if [ "$s" = "0" ]; then
        scripts/perf/bottleneck.sh 'sa-perf|stellar-archivist' 4 >> "$BN_LOG" 2>&1
      fi
      s=$((s+1))
    done
  fi
  wait "$RUN_PID"

  # Parse this cell's report for the backend-health signals.
  REP="perf-results/${RID}/report.json"
  if [ -f "$REP" ]; then
    read -r OK FAIL RETR < <(python3 - "$REP" <<'PY'
import json,sys
d=json.load(open(sys.argv[1]))["summary"]
print(d.get("succeeded",0), d.get("failed",0), d.get("retries",0))
PY
)
    echo "  [mc=$MC] succeeded=$OK failed=$FAIL retries=$RETR"
    if [ "${RETR:-0}" -gt "$RETRY_STOP" ]; then
      echo "  !! retries=$RETR > $RETRY_STOP — backend-health HARD STOP, ending sweep at mc=$MC"
      break
    fi
    if [ "${FAIL:-0}" -gt 0 ]; then
      echo "  !! failed=$FAIL > 0 — correctness/backend issue, ending sweep at mc=$MC"
      break
    fi
  else
    echo "  [mc=$MC] WARNING: no report.json"
  fi
done
echo "maxconc sweep [$LABEL/$MODE] done"
