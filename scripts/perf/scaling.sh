#!/usr/bin/env bash
# Usage: scaling.sh <fixture-url> [perf-binary]
# Env overrides:
#   SA_CS="1 2 4 8 16 32 64"   concurrency levels to sweep
#   SA_REPS=3                  repetitions per cell (report median + min/max)
#   SA_LOW / SA_HIGH           checkpoint range bounds (REQUIRED for a partial
#                              fixture — its .well-known advertises the tip, so
#                              an unbounded op errors on every missing genesis
#                              checkpoint). Passed as --low/--high to every run.
#   SA_TMP=/data/perf/tmp      parent dir for mirror destinations. MUST be on
#                              the big data volume, never the root disk.
#
# Sweeps -c across four modes — scan, scan --verify, mirror, mirror --verify —
# writing per-run artifacts (logs + report.json via run.sh) and
# perf-results/summary.csv. Repair is intentionally NOT in the scaling sweep:
# repair correctness is covered in Stage 1 (§5) and repair perf in the full
# pubnet stage (§6.2). The 2x2 here isolates existence-vs-verify (decompress+
# hash cost) and read-only-vs-read+write (scan vs mirror IO).
set -o pipefail
FIX="$1"; BIN="${2:-bin/sa-perf}"
CS="${SA_CS:-1 2 4 8 16 32 64}"; REPS="${SA_REPS:-3}"
TMP="${SA_TMP:-/data/perf/tmp}"; mkdir -p "$TMP"

BOUNDS=()
[ -n "${SA_LOW:-}" ]  && BOUNDS+=(--low "$SA_LOW")
[ -n "${SA_HIGH:-}" ] && BOUNDS+=(--high "$SA_HIGH")

rm -f perf-results/summary.csv
for C in $CS; do
  R=1
  while [ "$R" -le "$REPS" ]; do
    scripts/perf/run.sh "scan_C${C}_r${R}"        "$BIN" scan "$FIX" -c "$C" "${BOUNDS[@]}"
    scripts/perf/run.sh "scanverify_C${C}_r${R}"  "$BIN" scan "$FIX" -c "$C" --verify "${BOUNDS[@]}"

    D=$(mktemp -d -p "$TMP"); scripts/perf/run.sh "mirror_C${C}_r${R}" \
      "$BIN" mirror "$FIX" "file://$D" -c "$C" "${BOUNDS[@]}"; rm -rf "$D"
    D=$(mktemp -d -p "$TMP"); scripts/perf/run.sh "mirrorverify_C${C}_r${R}" \
      "$BIN" mirror "$FIX" "file://$D" -c "$C" --verify "${BOUNDS[@]}"; rm -rf "$D"

    R=$((R+1))
  done
done
echo "sweep done -> perf-results/summary.csv"
