#!/usr/bin/env bash
set -o pipefail
OUT="${1:-perf-results}"; mkdir -p "$OUT"
{
  echo "date: $(date -u +%FT%TZ)"
  echo "host: $(hostname)"
  echo "os: $(uname -a)"
  if [ "$(uname)" = "Darwin" ]; then
    echo "cpu: $(sysctl -n machdep.cpu.brand_string)"
    echo "cores_logical: $(sysctl -n hw.logicalcpu)"
    echo "cores_physical: $(sysctl -n hw.physicalcpu)"
    echo "ram_bytes: $(sysctl -n hw.memsize)"
  else
    echo "cpu: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs)"
    echo "cores_logical: $(nproc)"
    echo "ram_kb: $(awk '/MemTotal/{print $2}' /proc/meminfo)"
  fi
  echo "rustc: $(rustc --version)"
} | tee "$OUT/env.txt"
