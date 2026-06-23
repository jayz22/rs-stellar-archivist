#!/usr/bin/env bash
# capability_graph.sh — throughput-vs-cores for one binary. Runs the full 1xL10 verify
# (65,536 cp) to completion under a CPU-affinity cap (taskset --cpu-list 0..N-1) for each
# core count, so ALL threads (async workers, blocking pool, allocator) are confined to N
# physical cores. NO timeout. Appends one row per core count to perf-results/capability.csv.
#
# Usage: capability_graph.sh <label> <binary> "<core-list>"
#   e.g. capability_graph.sh A bin/sa-a "4 8 16 24 32"
# Env: SA_C (default 128) SA_MC (default 128) SA_LOW/SA_HIGH (default full 1xL10)
set -o pipefail
export PATH="$HOME/.cargo/bin:$PATH"
ROOT="/home/jay/Projects/rs-stellar-archivist-verifyperf"
cd "$ROOT" || exit 9
K="$1"; BIN="$2"; CORES="${3:-4 8 16 24 32}"
[ -x "$BIN" ] || { echo "no binary: $BIN"; exit 2; }
C="${SA_C:-128}"; MC="${SA_MC:-128}"
LOW="${SA_LOW:-58851711}"; HIGH="${SA_HIGH:-63046015}"
ARCHIVE="http://127.0.0.1:8088"
RES="$ROOT/perf-results/capability.csv"
[ -f "$RES" ] || echo "label,cores,wall_s,mb_per_s,cores_mean,cores_max,peak_rss_mb,succeeded,failed" > "$RES"

for N in $CORES; do
  RID="capability/${K}_n${N}"; DIR="$ROOT/perf-results/$RID"; mkdir -p "$DIR"
  LAST=$((N-1))
  echo "=== [$K n=$N cores] start $(date -u +%FT%TZ) (taskset 0-$LAST) ==="
  echo "taskset --cpu-list 0-$LAST $BIN scan ... -c $C mc $MC --verify" > "$DIR/cmd.txt"
  SA_RT_METRICS=1 taskset --cpu-list "0-$LAST" "$BIN" scan "$ARCHIVE" \
    -c "$C" --max-concurrent "$MC" --verify --skip-optional --low "$LOW" --high "$HIGH" \
    --report "$DIR/report.json" >"$DIR/stdout.log" 2>"$DIR/stderr.log"
  EX=$?
  # PERF line: wall_ms peak_rss_mb files bytes mb_per_s
  read -r WALL RSS MBPS < <(awk '/^PERF /{for(i=1;i<=NF;i++){split($i,a,"=");v[a[1]]=a[2]}; print v["wall_ms"],v["peak_rss_mb"],v["mb_per_s"]}' "$DIR/stderr.log" | tail -1)
  CORES_STAT=$(grep -o 'RTM busy_cores=[0-9.]*' "$DIR/stderr.log" | sed 's/.*=//' | \
    awk '{s+=$1;n++; if($1>mx)mx=$1} END{if(n)printf "%.1f %.1f",s/n,mx; else printf "0 0"}')
  read CM CX <<<"$CORES_STAT"
  read OK FAIL < <(python3 -c "import json,sys;d=json.load(open('$DIR/report.json'));s=d.get('summary',d);print(s.get('succeeded',0),s.get('failed',0))" 2>/dev/null || echo "? ?")
  WS=$(awk -v w="$WALL" 'BEGIN{if(w!="")printf"%.1f",w/1000}')
  echo "$K,$N,$WS,$MBPS,$CM,$CX,$RSS,$OK,$FAIL" >> "$RES"
  echo "=== [$K n=$N] done exit=$EX wall=${WS}s mbps=$MBPS cores(mean/max)=$CM/$CX ok=$OK fail=$FAIL ==="
done
echo "### capability [$K] DONE $(date -u +%FT%TZ) ###"
