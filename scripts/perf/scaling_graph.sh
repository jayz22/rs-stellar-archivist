#!/usr/bin/env bash
# scaling_graph.sh — throughput-vs-cores scaling graph for one binary. Runs the full
# 1xL10 verify (65,536 cp) to completion under a CPU-affinity cap (taskset --cpu-list
# 0..N-1) for each core count, so ALL threads (async workers, blocking pool, allocator)
# are confined to N physical cores. NO timeout. Appends one row per core count to
# perf-results/scaling_graph.csv.
#
# Usage: scaling_graph.sh <label> <binary> "<core-list>"
#   e.g. scaling_graph.sh A bin/sa-a "4 8 16 24 32"
# Env: SA_C (default 128) SA_MC (default 128) SA_LOW/SA_HIGH (default full 1xL10)
set -o pipefail
export PATH="$HOME/.cargo/bin:$PATH"
. "$(dirname "$0")/lib.sh"
# Run from the repo root; artifacts land under ./perf-results (gitignored).
K="$1"; BIN="$2"; CORES="${3:-4 8 16 24 32}"
[ -x "$BIN" ] || { echo "no binary: $BIN"; exit 2; }
C="${SA_C:-128}"; MC="${SA_MC:-128}"
LOW="${SA_LOW:-58851711}"; HIGH="${SA_HIGH:-63046015}"
ARCHIVE="${SA_ARCHIVE:-http://127.0.0.1:8088}"
RES="perf-results/scaling_graph.csv"; mkdir -p perf-results
HEADER="label,cores,wall_s,mb_per_s,cores_avg,worker_cores_mean,worker_cores_max,peak_rss_mb,succeeded,failed"
if [ ! -f "$RES" ] || [ "$(head -n 1 "$RES" 2>/dev/null)" != "$HEADER" ]; then
  echo "$HEADER" > "$RES"
fi

for N in $CORES; do
  RID="scaling_graph/${K}_n${N}"; DIR="perf-results/$RID"; mkdir -p "$DIR"
  LAST=$((N-1))
  echo "=== [$K n=$N cores] start $(date -u +%FT%TZ) (taskset 0-$LAST) ==="
  echo "taskset --cpu-list 0-$LAST $BIN scan ... -c $C mc $MC --verify" > "$DIR/cmd.txt"
  taskset --cpu-list "0-$LAST" "$BIN" scan "$ARCHIVE" \
    -c "$C" --max-concurrent "$MC" --verify --skip-optional --low "$LOW" --high "$HIGH" \
    --report "$DIR/report.json" >"$DIR/stdout.log" 2>"$DIR/stderr.log"
  EX=$?
  read -r WALL RSS MBPS < <(perf_fields "$DIR/stderr.log" PERF wall_ms peak_rss_mb measured_mb_per_s)
  CORES_AVG=$(perf_fields "$DIR/stderr.log" PERF_CPU cores_avg)
  CORES_STAT=$(grep -o 'RTM busy_cores=[0-9.]*' "$DIR/stderr.log" | sed 's/.*=//' | \
    awk '{s+=$1;n++; if($1>mx)mx=$1} END{if(n)printf "%.1f %.1f",s/n,mx; else printf "0 0"}')
  read CM CX <<<"$CORES_STAT"
  read -r OK FAIL _ < <(report_summary "$DIR/report.json" 2>/dev/null || echo "? ? ?")
  WS=$(awk -v w="$WALL" 'BEGIN{if(w!="")printf"%.1f",w/1000}')
  echo "$K,$N,$WS,$MBPS,$CORES_AVG,$CM,$CX,$RSS,$OK,$FAIL" >> "$RES"
  echo "=== [$K n=$N] done exit=$EX wall=${WS}s mbps=$MBPS cores(avg/worker-mean/max)=${CORES_AVG:-?}/$CM/$CX ok=$OK fail=$FAIL ==="
done
echo "### scaling graph [$K] DONE $(date -u +%FT%TZ) ###"
