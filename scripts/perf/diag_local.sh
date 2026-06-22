#!/usr/bin/env bash
# diag_local.sh — one comprehensive snapshot of a local-sim verify run, capturing
# BOTH the client (sa-verify) and server (miniserve) plus in-flight requests and
# disk read latency, so we can tell apart: client-concurrency limit vs server CPU
# vs disk-latency vs client decode. Window = $1 seconds (default 5).
#
# Usage: diag_local.sh [window_s]   (auto-resolves client+server pids by name)
set -o pipefail
W="${1:-5}"
CLK=$(getconf CLK_TCK); NCPU=$(nproc)
# resolve by exact comm (binary name truncates to 15 chars: "sa-verify-async"),
# NOT -f, which would match the zsh wrapper whose cmdline contains the args.
cpid=$(pgrep -x sa-verify-async | head -1)
[ -z "$cpid" ] && cpid=$(pgrep -x sa-verify-sync | head -1)
[ -z "$cpid" ] && cpid=$(pgrep -x stellar-archiv | head -1)
spid=$(pgrep -x miniserve | head -1)
[ -z "$cpid" ] && { echo "no client (sa-verify) running"; exit 1; }

pcpu() { awk '{print $14+$15}' "/proc/$1/stat" 2>/dev/null; }
dev_rx() { awk '/lo:/{gsub(/.*:/,"");print $1}' /proc/net/dev; }
disk() { awk '$3=="nvme1n1"||$3=="nvme2n1"{rd+=$4; rsec+=$6; rt+=$7; io+=$13} END{print rd, rsec, rt, io}' /proc/diskstats; }

c0=$(pcpu "$cpid"); s0=$(pcpu "${spid:-1}"); rx0=$(dev_rx); read -r drd0 drs0 drt0 dio0 <<<"$(disk)"; t0=$(cut -d' ' -f1 /proc/uptime)
sleep "$W"
c1=$(pcpu "$cpid"); s1=$(pcpu "${spid:-1}"); rx1=$(dev_rx); read -r drd1 drs1 drt1 dio1 <<<"$(disk)"; t1=$(cut -d' ' -f1 /proc/uptime)
el=$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.2f",b-a}')

# in-flight requests to :8088. Total ESTAB vs ACTIVE (non-empty recv/send queue =
# data actually in flight) — distinguishes real concurrent transfers from idle
# keep-alive pooled connections.
conn=$(ss -tnH '( dport = :8088 )' 2>/dev/null | grep -c ESTAB)
conn_active=$(ss -tnH '( dport = :8088 )' 2>/dev/null | awk '$1=="ESTAB" && ($2+0>0 || $3+0>0)' | wc -l)
# client thread states + what they wait on
cstates=$(for f in /proc/$cpid/task/*/stat; do awk '{print $3}' "$f" 2>/dev/null; done | sort | uniq -c | awk '{printf "%s=%s ",$2,$1}')
cwch=$(for f in /proc/$cpid/task/*/wchan; do v=$(cat "$f" 2>/dev/null); echo "${v:-run}"; done | sort | uniq -c | sort -rn | head -4 | awk '{printf "%s(%s) ",$2,$1}')
cthr=$(ls /proc/$cpid/task 2>/dev/null | wc -l)
# server thread states
sthr=$(ls /proc/${spid:-0}/task 2>/dev/null | wc -l)
sstates=$(for f in /proc/${spid:-0}/task/*/stat; do awk '{print $3}' "$f" 2>/dev/null; done | sort | uniq -c | awk '{printf "%s=%s ",$2,$1}')

awk -v c=$((c1-c0)) -v s=$((s1-s0)) -v clk="$CLK" -v el="$el" -v n="$NCPU" \
    -v rx=$((rx1-rx0)) -v drs=$((drs1-drs0)) -v drd=$((drd1-drd0)) -v drt=$((drt1-drt0)) -v dio=$((dio1-dio0)) \
    -v conn="$conn" -v conn_active="$conn_active" -v cthr="$cthr" -v sthr="$sthr" -v cst="$cstates" -v cw="$cwch" -v sst="$sstates" 'BEGIN{
  printf "CLIENT sa-verify : %.2f cores (%d thr)   states {%s} wait: %s\n", c/clk/el, cthr, cst, cw;
  printf "SERVER miniserve : %.2f cores (%d thr)   states {%s}\n", s/clk/el, sthr, sst;
  printf "IN-FLIGHT reqs   : %d ESTAB conns -> :8088 (%d ACTIVE w/ data queued)\n", conn, conn_active;
  printf "Throughput lo RX : %.1f MB/s\n", rx/1e6/el;
  printf "Disk read        : %.1f MB/s, %.0f reads/s, read-await %.2f ms, %%util %.0f%%\n",
         drs*512/1e6/el, drd/el, (drd>0?drt/drd:0), 100*dio/(el*1000) }'
