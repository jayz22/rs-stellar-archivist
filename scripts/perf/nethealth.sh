#!/usr/bin/env bash
# nethealth.sh — over one window, capture CONCRETE evidence of (a) how the box's
# cores are actually used, (b) what the app's threads are doing, and (c) the
# health of the connection to the remote archive (retransmits, timeouts, RTT).
#
# Designed for the --max-concurrent sweep: answers "are we using all N cores?",
# "where is the bottleneck?", and "is the public archive still healthy at this
# concurrency?" with numbers, not adjectives.
#
# Usage: nethealth.sh <pid> [window_s]
# No root required. TCP counter deltas are system-wide but attributable when the
# box is otherwise quiet (the only :443 traffic is this process).
set -o pipefail
PID="${1:?usage: nethealth.sh <pid> [window_s]}"
WIN="${2:-5}"
NCPU=$(nproc)
[ -d "/proc/$PID" ] || { echo "no live pid $PID"; exit 1; }

# ---- snapshot helpers -------------------------------------------------------
snmp_tcp()  { awk '/^Tcp:/{if($2=="RtoAlgorithm")next; print $13, $12}' /proc/net/snmp; }      # RetransSegs OutSegs
ext_tcp()   { awk '/^TcpExt:/{if(h==""){for(i=1;i<=NF;i++)k[$i]=i;h=1;next}
                    print $k["TCPTimeouts"], $k["TCPLostRetransmit"]+0, $k["TCPSynRetrans"]+0}' /proc/net/netstat; }
snmp_open() { awk '/^Tcp:/{if($2=="RtoAlgorithm")next; print $6, $8, $9}' /proc/net/snmp; }     # ActiveOpens AttemptFails EstabResets
percore()   { grep '^cpu[0-9]' /proc/stat | awk '{busy=$2+$3+$4+$6+$7+$8; idle=$5; print $1, busy, idle}'; }

# ---- t0 ----
read -r rs0 os0 <<<"$(snmp_tcp)"
read -r to0 lr0 sr0 <<<"$(ext_tcp)"
read -r ao0 af0 er0 <<<"$(snmp_open)"
declare -A CB0 CI0; while read -r c b i; do CB0[$c]=$b; CI0[$c]=$i; done < <(percore)
pu0=$(awk '{print $14+$15}' "/proc/$PID/stat" 2>/dev/null)
ts0=$(cut -d' ' -f1 /proc/uptime)

sleep "$WIN"

# ---- t1 ----
read -r rs1 os1 <<<"$(snmp_tcp)"
read -r to1 lr1 sr1 <<<"$(ext_tcp)"
read -r ao1 af1 er1 <<<"$(snmp_open)"
declare -A CB1 CI1; while read -r c b i; do CB1[$c]=$b; CI1[$c]=$i; done < <(percore)
pu1=$(awk '{print $14+$15}' "/proc/$PID/stat" 2>/dev/null)
ts1=$(cut -d' ' -f1 /proc/uptime)
EL=$(awk -v a="$ts0" -v b="$ts1" 'BEGIN{printf "%.2f", b-a}')
CLK=$(getconf CLK_TCK)

echo "=== nethealth: pid $PID ($(ps -o comm= -p "$PID"|xargs)), ${EL}s window, ${NCPU} cores ==="

# (A) CORES: how many of NCPU are actually doing work this window
busy_cores=0; sum_util=0
for c in "${!CB1[@]}"; do
  db=$(( ${CB1[$c]} - ${CB0[$c]} )); di=$(( ${CI1[$c]} - ${CI0[$c]} )); tot=$((db+di))
  [ "$tot" -le 0 ] && continue
  u=$(awk -v db="$db" -v t="$tot" 'BEGIN{printf "%.0f", 100*db/t}')
  sum_util=$(awk -v s="$sum_util" -v u="$u" 'BEGIN{print s+u}')
  [ "$u" -ge 50 ] && busy_cores=$((busy_cores+1))
done
proc_cores=$(awk -v d=$((pu1-pu0)) -v clk="$CLK" -v el="$EL" 'BEGIN{printf "%.2f", d/clk/el}')
echo "[A] Cores      : app uses ${proc_cores} cores of ${NCPU} | $(awk -v s="$sum_util" 'BEGIN{printf "%.1f", s/100}') core-equiv busy system-wide | ${busy_cores} core(s) >50%"

# (B) THREADS: running vs parked, and what the parked ones wait on
states=$(for t in /proc/$PID/task/*/stat; do awk '{print $3}' "$t" 2>/dev/null; done | sort | uniq -c | awk '{printf "%s=%s ",$2,$1}')
nthr=$(ls /proc/$PID/task 2>/dev/null | wc -l)
running=$(for t in /proc/$PID/task/*/stat; do awk '{print $3}' "$t" 2>/dev/null; done | grep -c R)
wchans=$(for t in /proc/$PID/task/*/wchan; do v=$(cat "$t" 2>/dev/null); echo "${v:-running}"; done | sort | uniq -c | sort -rn | head -4 | awk '{printf "%s(%s) ",$2,$1}')
echo "[B] Threads    : $nthr total, $running running | states {$states} | parked-on: $wchans"

# (C) CONNECTIONS to the archive (:443, this pid)
EST=$(ss -tnpH 2>/dev/null | grep -c "pid=$PID,")
SYN=$(ss -tnpH state syn-sent 2>/dev/null | grep -c "pid=$PID,")
echo "[C] Conns      : $EST established(:443) | $SYN syn-sent (connecting)"

# (D) RTT + per-socket retransmits, from this pid's :443 sockets (ss -i = kernel tcp_info).
#   Report MEDIAN smoothed-RTT (robust to the per-connection retransmit/delayed-ACK
#   outliers that skew a mean) AND minRTT (the true, concurrency-independent path
#   floor). See progress-report Appendix C for why the mean was misleading.
#   The (?<![a-z_]) guards stop "minrtt:"/"rcv_rtt:" from being read as the SRTT field.
ss -tinH "( dport = :443 )" 2>/dev/null | python3 -c '
import sys,re,statistics as st
t=sys.stdin.read()
srtt=[float(x) for x in re.findall(r"(?<![a-z_])rtt:([\d.]+)/",t)]
mn=[float(x) for x in re.findall(r"minrtt:([\d.]+)",t)]
rt=sum(int(b) for b in re.findall(r"(?<![a-z_])retrans:\d+/(\d+)",t))
if srtt:
    print("[D] RTT(:443)  : %d socks | SRTT median %.2f / max %.2f ms | minRTT(path) min %.2f / median %.2f ms | per-socket retrans total %d"%(
        len(srtt), st.median(srtt), max(srtt), (min(mn) if mn else 0), (st.median(mn) if mn else 0), rt))
else:
    print("[D] RTT(:443)  : (no active sockets in this snapshot)")
' 2>/dev/null || echo "[D] RTT(:443)  : (parse unavailable)"

# (E) ARCHIVE HEALTH: TCP retransmit/timeout/failure deltas over the window
awk -v rs=$((rs1-rs0)) -v os=$((os1-os0)) -v to=$((to1-to0)) -v lr=$((lr1-lr0)) -v sr=$((sr1-sr0)) \
    -v af=$((af1-af0)) -v er=$((er1-er0)) -v ao=$((ao1-ao0)) 'BEGIN{
  printf "[E] TCP health : retrans %d/%d segs (%.3f%%) | timeouts %d | lost-retrans %d | syn-retrans %d | attempt-fails %d | estab-resets %d | active-opens %d\n",
         rs, os, (os>0?100*rs/os:0), to, lr, sr, af, er, ao }'
