#!/usr/bin/env bash
# bottleneck.sh — sample EVERY resource dimension over one window for a running
# process + the system, then name the limiting dimension.
#
# Why: an op can be capped with CPU idle AND bandwidth unsaturated — the limiter
# is then latency/concurrency, disk IOPS/await, or lock contention. A single
# rate can't tell these apart on its own; you must look at all
# dimensions at once. This samples them over the same window and applies a
# priority heuristic.
#
# Dimensions: (1) CPU, (2) net bandwidth, (3) net requests/latency,
# (4) connections/concurrency, (5) disk bandwidth, (6) disk IOPS, (7) disk
# latency/await + D-state, (8) app concurrency via thread-state + wchan.
#
# Usage: bottleneck.sh [pid-or-pattern] [window_s]
# Env:   SA_NIC (default: default-route iface)  SA_DISKS (default: md0 members)
set -o pipefail
. "$(dirname "$0")/lib.sh"
PAT="${1:-sa-perf|sa-clean|stellar-archivist}"
WIN="${2:-5}"
NCPU=$(nproc); CLK=$(getconf CLK_TCK)
NIC="${SA_NIC:-$(ip route get 1.1.1.1 2>/dev/null | grep -oE 'dev [^ ]+' | awk '{print $2}')}"
DISKS="${SA_DISKS:-nvme1n1 nvme2n1}"

# Resolve the real binary pid (not the bash/time wrapper).
PID=$(resolve_pid "$PAT")
[ -z "${PID:-}" ] || [ ! -d "/proc/$PID" ] && { echo "no live process matching: $PAT"; exit 1; }

# ---- snapshot helpers -------------------------------------------------------
cpu_tot() { read -r _ a b c d _ < /proc/stat; echo "$((a+b+c+d)) $d"; }       # total idle
proc_cpu() { awk '{print $14+$15}' "/proc/$PID/stat" 2>/dev/null; }            # utime+stime ticks
net_dev() { awk -v n="$NIC:" '$1==n{gsub(/.*:/,"");print $1,$2,$9,$10}' /proc/net/dev; } # rxB rxP txB txP
disk_row() { awk -v d="$DISKS" 'BEGIN{split(d,a," ");for(i in a)w[a[i]]=1}
  $3 in w{rs+=$6; ws+=$10; rms+=$7; wms+=$11; rc+=$4; wc+=$8; io+=$13}
  END{print rc,wc,rs,ws,rms,wms,io}' /proc/diskstats; }                        # rc wc rsec wsec rms wms ioms

# ---- t0 ----
ts0=$(date +%s.%N)
read -r ct0 ci0 <<<"$(cpu_tot)"; pu0=$(proc_cpu)
read -r rb0 rp0 tb0 tp0 <<<"$(net_dev)"
read -r drc0 dwc0 drs0 dws0 drm0 dwm0 dio0 <<<"$(disk_row)"
sleep "$WIN"
ts1=$(date +%s.%N)
read -r ct1 ci1 <<<"$(cpu_tot)"; pu1=$(proc_cpu)
read -r rb1 rp1 tb1 tp1 <<<"$(net_dev)"
read -r drc1 dwc1 drs1 dws1 drm1 dwm1 dio1 <<<"$(disk_row)"
EL=$(awk -v a="$ts0" -v b="$ts1" 'BEGIN{printf "%.3f", b-a}')

# ---- connections (this pid, to :443) ----
CONN_EST=$(ss -tnpH 2>/dev/null | grep -c "pid=$PID,")
CONN_SYN=$(ss -tnpH state syn-sent 2>/dev/null | grep -c "pid=$PID,")
CONN_TW=$(ss -tnH state time-wait '( dport = :443 )' 2>/dev/null | wc -l)

# ---- thread states + wchan ----
states=$(for t in /proc/$PID/task/*/stat; do awk '{print $3}' "$t" 2>/dev/null; done | sort | uniq -c | sort -rn | awk '{printf "%s=%s ",$2,$1}')
wchans=$(for t in /proc/$PID/task/*/wchan; do v=$(cat "$t" 2>/dev/null); echo "${v:-running}"; done | sort | uniq -c | sort -rn | head -4 | awk '{printf "%s(%s) ",$2,$1}')

echo "=== bottleneck probe: pid $PID ($(ps -o comm= -p "$PID"|xargs)), $(ls /proc/$PID/task|wc -l) threads, ${NCPU} cores, ${EL}s window ==="

# (1) CPU
awk -v dt=$((ct1-ct0)) -v di=$((ci1-ci0)) -v n="$NCPU" 'BEGIN{b=100*(dt-di)/dt; printf "[1] CPU        : sys %.1f%% busy (~%.1f/%d cores)", b, b/100*n, n}'
awk -v d=$((pu1-pu0)) -v clk="$CLK" -v el="$EL" 'BEGIN{printf "  | proc %.2f cores\n", d/clk/el}'
# (2)(3) network
awk -v rb=$((rb1-rb0)) -v rp=$((rp1-rp0)) -v tb=$((tb1-tb0)) -v tp=$((tp1-tp0)) -v el="$EL" 'BEGIN{
  printf "[2] Net BW     : RX %.1f MB/s, TX %.1f MB/s\n", rb/1e6/el, tb/1e6/el;
  printf "[3] Net reqs   : RX %.0f pkt/s (avg %.0f B/pkt), TX %.0f pkt/s\n", rp/el, (rp>0?rb/rp:0), tp/el }'
# (4) connections
echo "[4] Conns      : $CONN_EST established(:443, this pid) | syn-sent $CONN_SYN | time-wait(:443,all) $CONN_TW"
# (5)(6)(7) disk
awk -v ws=$((dws1-dws0)) -v wc=$((dwc1-dwc0)) -v wms=$((dwm1-dwm0)) -v rs=$((drs1-drs0)) -v rc=$((drc1-drc0)) -v io=$((dio1-dio0)) -v el="$EL" 'BEGIN{
  printf "[5] Disk BW    : write %.1f MB/s, read %.1f MB/s (%s)\n", ws*512/1e6/el, rs*512/1e6/el, "'"$DISKS"'";
  printf "[6] Disk IOPS  : %.0f write/s, %.0f read/s\n", wc/el, rc/el;
  printf "[7] Disk lat   : %%util %.0f%%, write await %.2f ms\n", 100*io/(el*1000), (wc>0?wms/wc:0) }'
# (8) app concurrency
echo "[8] Threads    : states { $states} | blocked-on: $wchans"

# ---- verdict heuristic ----
read -r VERD <<EOF
$(awk -v dt=$((ct1-ct0)) -v di=$((ci1-ci0)) -v n="$NCPU" -v io=$((dio1-dio0)) -v el="$EL" \
      -v ws=$((dws1-dws0)) -v wc=$((dwc1-dwc0)) -v wms=$((dwm1-dwm0)) -v rb=$((rb1-rb0)) 'BEGIN{
  sysb=100*(dt-di)/dt; util=100*io/(el*1000); rxmb=rb/1e6/el; aw=(wc>0?wms/wc:0);
  if (sysb > 60) print "CPU-bound (cores saturated)";
  else if (util > 80 && ws*512/1e6/el < 300) print "DISK-IOPS/metadata-bound (disk busy at low MB/s — small-file writes)";
  else if (util > 80) print "DISK-bandwidth-bound";
  else if (aw > 20) print "DISK-latency-bound (high await)";
  else if (rxmb > 180) print "NETWORK-bandwidth-bound (near single-stream ceiling)";
  else print "NETWORK-LATENCY / CONCURRENCY-bound (CPU idle, disk idle, BW < ceiling => waiting on round-trips or locks; check [3] pps, [4] conns, [8] futex)";
}')
EOF
echo "=== VERDICT: $VERD ==="
