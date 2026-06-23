#!/usr/bin/env bash
# bench_strategies.sh — Phase-2 cross-strategy perf for the verify-scaling
# investigation. Runs ONE strategy binary on the full 65,536-cp (1×L10) range,
# verify + no-verify, with SA_RT_METRICS core sampling, and appends a parsed
# row to perf-results/phase2_results.csv. Run ONE at a time (no concurrent perf,
# no builds during a run). NO timeout — runs to completion.
#
# Usage: bench_strategies.sh <strategy-label> <binary> [archive-url]
# Env:   SA_C (default 128)  SA_MC (default 128)
#        SA_LOW SA_HIGH (default full 1xL10: 58851711..63046015)
set -o pipefail
export PATH="$HOME/.cargo/bin:$PATH"
ROOT="/home/jay/Projects/rs-stellar-archivist-verifyperf"
cd "$ROOT" || exit 9
K="$1"; BIN="$2"; ARCHIVE="${3:-http://127.0.0.1:8088}"
[ -x "$BIN" ] || { echo "no binary: $BIN"; exit 2; }
C="${SA_C:-128}"; MC="${SA_MC:-128}"
LOW="${SA_LOW:-58851711}"; HIGH="${SA_HIGH:-63046015}"
RES="$ROOT/perf-results/phase2_results.csv"
[ -f "$RES" ] || echo "strategy,mode,wall_s,mb_per_s,peak_rss_mb,rss_os_mb,cores_mean,cores_p50,cores_max,active_dec_mean,succeeded,failed,retries" > "$RES"

parse_cores() {  # $1 = stderr.log -> "mean p50 max admean"
  grep -o 'RTM busy_cores=[0-9.]*' "$1" 2>/dev/null | sed 's/.*=//' > /tmp/_cores_$$ || true
  grep -o 'active_decodes=[0-9]*' "$1" 2>/dev/null | sed 's/.*=//' > /tmp/_adec_$$ || true
  python3 - /tmp/_cores_$$ /tmp/_adec_$$ <<'PY'
import sys
def stats(f):
    try: v=[float(x) for x in open(f) if x.strip()]
    except: v=[]
    if not v: return (0,0,0)
    s=sorted(v); n=len(s)
    return (sum(v)/n, s[n//2], max(v))
m,p,mx=stats(sys.argv[1])
am,_,_=stats(sys.argv[2])
print(f"{m:.1f} {p:.1f} {mx:.1f} {am:.1f}")
PY
  rm -f /tmp/_cores_$$ /tmp/_adec_$$
}

read_summary() {  # $1 = report.json -> "ok fail retr"
  python3 - "$1" <<'PY'
import json,sys
try:
    d=json.load(open(sys.argv[1]))
    s=d.get("summary",d)
    print(s.get("succeeded",0), s.get("failed",0), s.get("retries",0))
except Exception:
    print("? ? ?")
PY
}

run_cell() {  # $1=mode (verify|noverify)
  local MODE="$1"; local RID="${K}/${MODE}"; local DIR="$ROOT/perf-results/$RID"
  local VFLAG=(); [ "$MODE" = "verify" ] && VFLAG=(--verify)
  echo "=== [$K/$MODE] start $(date -u +%FT%TZ) range=$LOW..$HIGH -c=$C mc=$MC ==="
  SA_RT_METRICS=1 scripts/perf/run.sh "$RID" "$BIN" scan "$ARCHIVE" \
    -c "$C" --max-concurrent "$MC" "${VFLAG[@]}" --skip-optional --low "$LOW" --high "$HIGH"
  local W MBPS RSS FILES BYTES
  if [ -f "$DIR/headline.csv" ]; then
    IFS=, read -r W RSS FILES BYTES MBPS < <(tail -1 "$DIR/headline.csv")
  fi
  # peak_rss_mb_os is column 5 of summary.csv for this run id (last matching row).
  local RSS_OS; RSS_OS=$(awk -F, -v rid="$RID" '$1==rid{v=$5} END{print v}' "$ROOT/perf-results/summary.csv")
  local CORES; CORES=$(parse_cores "$DIR/stderr.log")
  read CM CP CX CAD <<<"$CORES"
  read OK FAIL RETR <<<"$(read_summary "$DIR/report.json")"
  local WS; WS=$(awk -v w="$W" 'BEGIN{if(w=="")print"";else printf"%.1f",w/1000}')
  echo "$K,$MODE,$WS,$MBPS,$RSS,$RSS_OS,$CM,$CP,$CX,$CAD,$OK,$FAIL,$RETR" >> "$RES"
  echo "=== [$K/$MODE] done wall=${WS}s mbps=$MBPS rss=${RSS}MB cores(mean/p50/max)=$CM/$CP/$CX active_dec=$CAD ok=$OK fail=$FAIL ==="
}

run_cell verify
run_cell noverify
echo "### [$K] BOTH CELLS DONE $(date -u +%FT%TZ) ###"
