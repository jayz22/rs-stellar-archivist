#!/usr/bin/env bash
# clean_sweep.sh — verify CPU-scaling re-measurement on a QUIET box.
# 2x2 variant matrix (decode path x gzip backend), scan-verify c=1/4/16, 1 rep.
# Samples bottleneck.sh at c=16 per variant (the "did we use the cores" evidence).
# Reuses run.sh (OS /usr/bin/time wall+RSS + in-process sa-perf headline/phases).
set -o pipefail
cd /workspaces/rs-stellar-archivist || exit 1
FIX=file:///data/perf/fixture
LOW=62918015 HIGH=63046015
BASE=perf-results/verifyperf/clean
mkdir -p "$BASE"

echo "==== SWEEP START $(date -u +%FT%TZ) ===="
echo "[quiet check] competing sa/mirror procs:"; pgrep -af 'mirror|stellar-archivist' | grep -v clean_sweep || echo "  none"

echo "[warm] reading fixture into page cache..."
find /data/perf/fixture -type f -print0 | xargs -0 cat > /dev/null
free -h

# variant -> binary
names="baseline zrs sync sync-zrs"
bin_baseline=bin/sa-perf            # async  + miniz   (pre-Phase-2, 6e5e58e)
bin_zrs=bin/sa-perf-zrs             # async  + zlib-rs
bin_sync=bin/sa-perf-sync           # sync   + miniz   (Phase 2, HEAD)
bin_synczrs=bin/sa-perf-sync-zrs    # sync   + zlib-rs (combo)

for v in $names; do
  case "$v" in
    baseline) b=$bin_baseline;; zrs) b=$bin_zrs;;
    sync) b=$bin_sync;; sync-zrs) b=$bin_synczrs;;
  esac
  mkdir -p "$BASE/$v"
  for c in 1 4 16; do
    RID="verifyperf/clean/$v/scanverify_C${c}_r1"
    echo "[run] $RID  ($b -c $c)  $(date -u +%FT%TZ)"
    if [ "$c" = "16" ]; then
      scripts/perf/run.sh "$RID" "$b" scan "$FIX" --verify --low $LOW --high $HIGH -c $c &
      RUNPID=$!
      BL="$BASE/$v/bottleneck_C16.txt"; : > "$BL"
      sleep 25
      for i in 1 2 3 4; do
        kill -0 "$RUNPID" 2>/dev/null || break
        echo "=== bottleneck sample $i  $(date -u +%FT%TZ) ===" >> "$BL"
        SA_DISKS="nvme0n1 nvme1n1" scripts/perf/bottleneck.sh 'sa-perf|stellar-archivist' 5 >> "$BL" 2>&1
        sleep 20
      done
      wait "$RUNPID"
    else
      scripts/perf/run.sh "$RID" "$b" scan "$FIX" --verify --low $LOW --high $HIGH -c $c
    fi
  done
done
echo "==== SWEEP DONE $(date -u +%FT%TZ) ===="
