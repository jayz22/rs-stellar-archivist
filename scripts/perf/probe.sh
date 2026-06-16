#!/usr/bin/env bash
# probe.sh — live resource-bottleneck snapshot of a running process.
#
# Complements run.sh/scaling.sh (which capture wall + peak RSS + phases at EXIT)
# by answering, mid-run: where is this bottlenecked RIGHT NOW — CPU or I/O?
#
# Method (a "USE-method" snapshot): sample each resource's utilization over a
# short window and compare to its ceiling. The saturated resource while others
# idle is the bottleneck.
#   - system CPU: delta of idle/total from two /proc/stat reads (instantaneous,
#     unlike ps/top %CPU which averages over the whole process lifetime)
#   - process CPU: delta of utime+stime from /proc/<pid>/stat
#   - load average: corroborates (CPU-bound on N cores => load ~N)
#   - throughput: optional dir size-delta, to compare against a known ceiling
#     (e.g. ~203 MB/s single-stream network here; exceeding it => parallel I/O)
#
# Usage: probe.sh [pid-or-pattern] [growth-dir] [interval_s]
#   pid-or-pattern : a PID, or a pgrep -f pattern (default the archivist bins)
#   growth-dir     : optional dir whose byte-growth = write throughput
#   interval_s     : sampling window in seconds (default 5)
#
# Examples:
#   scripts/perf/probe.sh 'sa-perf mirror' /data/pubnet-mirror 10
#   scripts/perf/probe.sh 12345
set -o pipefail
PAT="${1:-sa-perf|sa-clean|stellar-archivist|corrupt-archive}"
DIR="${2:-}"; INT="${3:-5}"
NCPU=$(nproc); CLK=$(getconf CLK_TCK)

if [[ "$PAT" =~ ^[0-9]+$ ]]; then PID="$PAT"; else PID=$(pgrep -f "$PAT" | head -1); fi
[ -z "${PID:-}" ] && { echo "no process matching: $PAT"; exit 1; }
[ -d "/proc/$PID" ] || { echo "pid $PID not alive"; exit 1; }

echo "target : pid $PID ($(ps -o comm= -p "$PID" | xargs)), $(ps -o nlwp= -p "$PID" | xargs) threads, ${NCPU} cores"

read -r _ a b c idle _ < /proc/stat; t1=$((a+b+c+idle)); i1=$idle
pu1=$(awk '{print $14+$15}' "/proc/$PID/stat" 2>/dev/null)
[ -n "$DIR" ] && s1=$(du -sb "$DIR" 2>/dev/null | cut -f1)
sleep "$INT"
read -r _ a b c idle _ < /proc/stat; t2=$((a+b+c+idle)); i2=$idle
pu2=$(awk '{print $14+$15}' "/proc/$PID/stat" 2>/dev/null)

awk -v dt=$((t2-t1)) -v di=$((i2-i1)) -v n="$NCPU" \
  'BEGIN{busy=100*(dt-di)/dt; printf "sys CPU: %.1f%% busy / %.1f%% idle  (~%.1f of %d cores)\n", busy, 100-busy, busy/100*n, n}'
awk -v d=$((pu2-pu1)) -v clk="$CLK" -v int="$INT" \
  'BEGIN{printf "proc CPU: %.0f%%  (%.2f cores-equiv)\n", 100*d/clk/int, d/clk/int}'
echo "loadavg: $(cut -d' ' -f1-3 /proc/loadavg)  (CPU-bound on ${NCPU} cores => load ~${NCPU})"
echo "proc RSS: $(awk '/VmRSS/{printf "%.0f MB (current, not peak)", $2/1024}' "/proc/$PID/status" 2>/dev/null)"
if [ -n "$DIR" ]; then
  s2=$(du -sb "$DIR" 2>/dev/null | cut -f1)
  awk -v d=$((s2-s1)) -v int="$INT" -v dir="$DIR" \
    'BEGIN{printf "I/O    : %.1f MB/s into %s\n", d/1e6/int, dir}'
fi
echo "verdict: low sys-CPU% + steady I/O => I/O/network-bound; sys-CPU% ~= 100%*cores => CPU-bound."
