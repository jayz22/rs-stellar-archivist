#!/usr/bin/env bash
# verify_subsweep.sh — scan-verify subset sweep for the verify-speedup experiment.
# Reuses run.sh (OS time + in-process sa-perf headline/phases). Results land under
# perf-results/<LABEL>/scanverify_C<c>_r<rep>/ and the shared perf-results/summary.csv.
#
# Usage: verify_subsweep.sh <label> <binary> [probe_at_c]
#   label      : subdir under perf-results/ (e.g. verifyperf/baseline)
#   binary     : path to sa-perf-style binary
#   probe_at_c : if set, run probe.sh during the r1 run at this -c (e.g. 16)
#
# Env: SA_CS (default "1 4 16"), SA_REPS (default 2), FIX, SA_LOW, SA_HIGH.
set -o pipefail
export PATH="$HOME/.cargo/bin:$PATH"
LABEL="$1"; BIN="$2"; PROBE_AT="${3:-}"
FIX="${FIX:-file:///data/perf/fixture}"
CS="${SA_CS:-1 4 16}"; REPS="${SA_REPS:-2}"
LOW="${SA_LOW:-62918015}"; HIGH="${SA_HIGH:-63046015}"

for C in $CS; do
  R=1
  while [ "$R" -le "$REPS" ]; do
    RID="${LABEL}/scanverify_C${C}_r${R}"
    if [ "$C" = "$PROBE_AT" ] && [ "$R" = "1" ]; then
      # Launch the run in the background so we can sample probe.sh mid-flight.
      ( scripts/perf/run.sh "$RID" "$BIN" scan "$FIX" -c "$C" --verify \
          --low "$LOW" --high "$HIGH" ) &
      RUN_PID=$!
      PROBE_LOG="perf-results/${LABEL}/probe_C${C}.txt"
      : > "$PROBE_LOG"
      # Sample several times across the run's life.
      sleep 60
      for i in 1 2 3 4 5; do
        kill -0 "$RUN_PID" 2>/dev/null || break
        echo "--- probe sample $i (t~$((60+i*60))s) ---" >> "$PROBE_LOG"
        scripts/perf/probe.sh 'sa-perf|sa-clean|stellar-archivist' "" 5 >> "$PROBE_LOG" 2>&1
        sleep 55
      done
      wait "$RUN_PID"
    else
      scripts/perf/run.sh "$RID" "$BIN" scan "$FIX" -c "$C" --verify \
        --low "$LOW" --high "$HIGH"
    fi
    R=$((R+1))
  done
done
echo "subsweep [$LABEL] done"
