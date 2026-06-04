#!/usr/bin/env bash
# Usage: run.sh <run-id> <binary> <op> [args...]
# Runs the binary, captures in-process perf metrics (when built with
# --features perf-metrics) plus an optional OS /usr/bin/time RSS cross-check,
# and appends one row to perf-results/summary.csv.
set -o pipefail
RID="$1"; BIN="$2"; shift 2
ROOT="perf-results/$RID"; mkdir -p "$ROOT"
export SA_PERF_OUT="$ROOT"
echo "$BIN $*" > "$ROOT/cmd.txt"

TIMER=(); RSS_DIV=1
if [ "$(uname)" = "Darwin" ] && [ -x /usr/bin/time ]; then
  TIMER=(/usr/bin/time -l); RSS_DIV=1048576           # macOS: bytes
elif [ -x /usr/bin/time ] && /usr/bin/time -v true >/dev/null 2>&1; then
  TIMER=(/usr/bin/time -v); RSS_DIV=1024              # GNU time: kbytes
fi

"${TIMER[@]}" "$BIN" "$@" >"$ROOT/stdout.log" 2>"$ROOT/stderr.log"
EXIT=$?

WALL=""; RSS=""; FILES=""; BYTES=""; MBPS=""
if [ -f "$ROOT/headline.csv" ]; then
  IFS=, read -r WALL RSS FILES BYTES MBPS < <(tail -1 "$ROOT/headline.csv")
fi
RSS_OS=""
if [ ${#TIMER[@]} -gt 0 ]; then
  if [ "$(uname)" = "Darwin" ]; then
    P=$(grep 'maximum resident set size' "$ROOT/stderr.log" | awk '{print $1}')
  else
    P=$(grep 'Maximum resident set size' "$ROOT/stderr.log" | awk -F': ' '{print $2}')
  fi
  if [ -n "$P" ]; then RSS_OS=$(awk -v p="$P" -v d="$RSS_DIV" 'BEGIN{printf "%.1f", p/d}'); fi
fi
if [ ! -f perf-results/summary.csv ]; then
  echo "run_id,exit,wall_ms,peak_rss_mb,peak_rss_mb_os,files,bytes,mb_per_s" > perf-results/summary.csv
fi
echo "$RID,$EXIT,$WALL,$RSS,$RSS_OS,$FILES,$BYTES,$MBPS" >> perf-results/summary.csv
echo "[$RID] exit=$EXIT wall_ms=${WALL:-?} rss_mb=${RSS:-?} (os=${RSS_OS:-?}) files=${FILES:-?}"
exit 0
