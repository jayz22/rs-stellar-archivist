#!/usr/bin/env bash
# Stage 1 correctness (plan §5): scan / mirror / repair / plan-driven repair
# against a small complete local archive.
#
# Usage: stage1.sh <src-dir> <label>
#   e.g. stage1.sh testdata/testnet-archive-small v1
#        stage1.sh testdata/testnet-archive-v2    v2
#
# Artifacts land in perf-results/stage1/<label>/. Prints §-numbered PASS lines;
# exits non-zero on the first failure.
set -uo pipefail
SRCDIR="$1"; LABEL="$2"
SA=bin/sa-clean; CORRUPT=bin/corrupt-archive; CHECK="python3 scripts/perf/check_stage1.py"
SRC="file://$PWD/$SRCDIR"
WORK="perf-results/stage1/$LABEL"; rm -rf "$WORK"; mkdir -p "$WORK"
MIR="$PWD/$WORK/mirror"; SNAP="$PWD/$WORK/snapshot"
die() { echo "FAIL [$LABEL] $*"; exit 1; }
note() { echo "== [$LABEL] $*"; }

# --- 5.1 scan finds everything -------------------------------------------
note "5.1 scan --verify"
$SA scan "$SRC" --verify --report "$WORK/scan.json" >"$WORK/scan.log" 2>&1 \
  || die "5.1 scan exit $?"
$CHECK inventory "$WORK/scan.json" "$SRCDIR" || die "5.1 inventory"

# --- 5.2 mirror reproduces src -------------------------------------------
note "5.2 mirror --verify + diff"
$SA mirror "$SRC" "file://$MIR" --verify --report "$WORK/mirror.json" >"$WORK/mirror.log" 2>&1 \
  || die "5.2 mirror exit $?"
diff -r "$SRCDIR" "$MIR" > "$WORK/mirror.diff" 2>&1
if [ -s "$WORK/mirror.diff" ]; then
  # acceptable iff the ONLY diff is .well-known AND it matches the rewrite rule
  grep -v '.well-known' "$WORK/mirror.diff" | grep -q . \
    && die "5.2 mirror.diff has non-well-known differences"
  $CHECK wellknown "$MIR" || die "5.2 well-known rewrite rule"
fi
echo "PASS: [5.2] mirror reproduces src"
cp -r "$MIR" "$SNAP"

# --- 5.3 repair restores a corrupted copy --------------------------------
note "5.3 corrupt(20,seed1) -> detect -> repair -> diff"
$CORRUPT "$MIR" --kinds all --count 20 --seed 1 --manifest "$WORK/corrupt.json" \
  >"$WORK/corrupt.log" 2>&1 || die "5.3 corrupt-archive exit $?"
$SA scan "file://$MIR" --verify --report "$WORK/scan-corrupt.json" >"$WORK/scan-corrupt.log" 2>&1
SCAN_EXIT=$?
[ "$SCAN_EXIT" -ne 0 ] || die "5.3 corrupted scan unexpectedly clean (exit 0)"
echo "PASS: [5.3a] corruption detected (scan exit $SCAN_EXIT)"
if [ -s "$WORK/scan-corrupt.json" ]; then
  $CHECK broken-match "$WORK/scan-corrupt.json" "$WORK/corrupt.json" \
    || die "5.3a scan broken set mismatch"
fi
$SA repair "$SRC" "file://$MIR" --verify --report "$WORK/repair.json" >"$WORK/repair.log" 2>&1 \
  || die "5.3 repair exit $?"
diff -r "$SNAP" "$MIR" > "$WORK/repair.diff" 2>&1
[ -s "$WORK/repair.diff" ] && die "5.3 repair.diff not empty"
echo "PASS: [5.3c] repair restored snapshot (diff empty)"
$CHECK repair-match "$WORK/repair.json" "$WORK/corrupt.json" || die "5.3d repair broken set mismatch"

# --- 5.4 dry-run plan + apply --------------------------------------------
note "5.4 corrupt(10,seed2) -> dry-run --verify plan -> apply --plan -> diff"
COPY="$PWD/$WORK/copy2"; cp -r "$SNAP" "$COPY"
$CORRUPT "$COPY" --kinds all --count 10 --seed 2 --manifest "$WORK/c2.json" \
  >"$WORK/c2.log" 2>&1 || die "5.4 corrupt-archive exit $?"
$SA repair "$SRC" "file://$COPY" --dry-run --verify --report "$WORK/plan.json" >"$WORK/plan.log" 2>&1 \
  || die "5.4 dry-run exit $?"
[ -s "$WORK/plan.json" ] || die "5.4 dry-run produced no plan"
$SA repair "$SRC" "file://$COPY" --plan "$WORK/plan.json" >"$WORK/apply.log" 2>&1 \
  || die "5.4 apply --plan exit $?"
diff -r "$SNAP" "$COPY" > "$WORK/plan-apply.diff" 2>&1
[ -s "$WORK/plan-apply.diff" ] && die "5.4 plan-apply.diff not empty"
echo "PASS: [5.4] plan-driven repair restored snapshot (diff empty)"

echo "== [$LABEL] Stage 1 ALL PASS =="
