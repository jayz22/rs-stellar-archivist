#!/usr/bin/env bash
# Usage: run.sh <run-id> <binary> <op> [args...]
# Runs the binary, captures in-process perf metrics (when built with
# --features perf-metrics) plus an optional OS /usr/bin/time RSS cross-check,
# and appends one row to perf-results/summary.csv.
set -o pipefail
. "$(dirname "$0")/lib.sh"
RID="$1"; BIN="$2"; shift 2
ROOT="perf-results/$RID"; mkdir -p "$ROOT"

# Auto-inject --report into this run's dir unless the caller already passed one,
# so every run keeps its own JSON report alongside the logs for later review.
# (scan/mirror/repair all accept --report.)
ARGS=("$@")
HAS_REPORT=0
for a in "${ARGS[@]}"; do [ "$a" = "--report" ] && HAS_REPORT=1 && break; done
if [ "$HAS_REPORT" -eq 0 ]; then
  ARGS+=(--report "$ROOT/report.json")
fi
echo "$BIN ${ARGS[*]}" > "$ROOT/cmd.txt"

TIMER=(); RSS_DIV=1
if [ "$(uname)" = "Darwin" ] && [ -x /usr/bin/time ]; then
  TIMER=(/usr/bin/time -l); RSS_DIV=1048576           # macOS: bytes
elif [ -x /usr/bin/time ] && /usr/bin/time -v true >/dev/null 2>&1; then
  TIMER=(/usr/bin/time -v); RSS_DIV=1024              # GNU time: kbytes
fi

"${TIMER[@]}" "$BIN" "${ARGS[@]}" >"$ROOT/stdout.log" 2>"$ROOT/stderr.log"
EXIT=$?

read -r WALL RSS FILES BYTES MBPS < <(perf_fields "$ROOT/stderr.log" PERF \
  wall_ms peak_rss_mb files_measured measured_bytes measured_mb_per_s)
read -r CPU_MS CORES_AVG < <(perf_fields "$ROOT/stderr.log" PERF_CPU total_cpu_ms cores_avg)
read -r HB_MAX_US HB_OVER_10MS < <(perf_fields "$ROOT/stderr.log" PERF_HEARTBEAT max_us over_10ms)
RSS_OS=""
if [ ${#TIMER[@]} -gt 0 ]; then
  if [ "$(uname)" = "Darwin" ]; then
    P=$(grep 'maximum resident set size' "$ROOT/stderr.log" | awk '{print $1}')
  else
    P=$(grep 'Maximum resident set size' "$ROOT/stderr.log" | awk -F': ' '{print $2}')
  fi
  if [ -n "$P" ]; then RSS_OS=$(awk -v p="$P" -v d="$RSS_DIV" 'BEGIN{printf "%.1f", p/d}'); fi
fi
SUMMARY="perf-results/summary.csv"
HEADER="run_id,exit,wall_ms,peak_rss_mb,peak_rss_mb_os,files_measured,measured_bytes,measured_mb_per_s,total_cpu_ms,cores_avg,heartbeat_max_us,heartbeat_over_10ms"
if [ ! -f "$SUMMARY" ] || [ "$(head -n 1 "$SUMMARY" 2>/dev/null)" != "$HEADER" ]; then
  echo "$HEADER" > "$SUMMARY"
fi
echo "$RID,$EXIT,$WALL,$RSS,$RSS_OS,$FILES,$BYTES,$MBPS,$CPU_MS,$CORES_AVG,$HB_MAX_US,$HB_OVER_10MS" >> "$SUMMARY"
echo "[$RID] exit=$EXIT wall_ms=${WALL:-?} rss_mb=${RSS:-?} (os=${RSS_OS:-?}) files=${FILES:-?} cores=${CORES_AVG:-?} hb_max_us=${HB_MAX_US:-?} hb_over_10ms=${HB_OVER_10MS:-?}"
exit 0
