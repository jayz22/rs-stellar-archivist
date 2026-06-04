#!/usr/bin/env bash
# Usage: scaling.sh <fixture-url> [perf-binary] [corrupt-binary]
# Env overrides: SA_CS="1 2 4 8 16 32 64"  SA_REPS=2
# Sweeps -C across modes (scan, scan-verify, mirror, mirror-verify, repair, repair-dry-run),
# writing per-run artifacts and perf-results/summary.csv.
set -o pipefail
FIX="$1"; BIN="${2:-bin/sa-perf}"; CORRUPT="${3:-bin/corrupt-archive}"
CS="${SA_CS:-1 2 4 8 16 32 64}"; REPS="${SA_REPS:-2}"
FX_DIR="${FIX#file://}"
rm -f perf-results/summary.csv
for C in $CS; do
  R=1
  while [ "$R" -le "$REPS" ]; do
    scripts/perf/run.sh "scan_C${C}_r${R}"        "$BIN" scan   "$FIX" -c "$C"
    scripts/perf/run.sh "scanverify_C${C}_r${R}"  "$BIN" scan   "$FIX" -c "$C" --verify
    D=$(mktemp -d); scripts/perf/run.sh "mirror_C${C}_r${R}"       "$BIN" mirror "$FIX" "file://$D" -c "$C"; rm -rf "$D"
    D=$(mktemp -d); scripts/perf/run.sh "mirrorverify_C${C}_r${R}" "$BIN" mirror "$FIX" "file://$D" -c "$C" --verify; rm -rf "$D"
    D=$(mktemp -d); cp -r "$FX_DIR" "$D/a"; "$CORRUPT" "$D/a" --kinds all --count 20 --seed 99 >/dev/null 2>&1
    scripts/perf/run.sh "repairdry_C${C}_r${R}" "$BIN" repair "$FIX" "file://$D/a" -c "$C" --dry-run
    scripts/perf/run.sh "repair_C${C}_r${R}"    "$BIN" repair "$FIX" "file://$D/a" -c "$C"
    rm -rf "$D"
    R=$((R+1))
  done
done
echo "sweep done -> perf-results/summary.csv"
