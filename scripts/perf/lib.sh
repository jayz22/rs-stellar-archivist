#!/usr/bin/env bash
# Shared helpers for the perf harness. Sourced (not executed) by the other
# scripts to remove three idioms that were previously copy-pasted across
# run.sh, scaling_graph.sh, verify_scaling.sh, and bottleneck.sh.

# resolve_pid <pgrep-pattern|pid> -> echo the real binary PID, or return 1.
# A bare number is echoed back unchanged; otherwise pgrep -f, skipping the
# bash/time/pgrep wrappers, with a head-1 fallback if only wrappers matched.
resolve_pid() {
  local pat="$1" p
  [[ "$pat" =~ ^[0-9]+$ ]] && { echo "$pat"; return 0; }
  for p in $(pgrep -f "$pat"); do
    case "$(ps -o comm= -p "$p" 2>/dev/null | xargs)" in
      bash|sh|zsh|dash|time|nohup|pgrep) continue ;;
    esac
    echo "$p"; return 0
  done
  p=$(pgrep -f "$pat" | head -1); [ -n "$p" ] && { echo "$p"; return 0; }
  return 1
}

# perf_fields <logfile> <tag> <key>... -> the requested values (space-separated)
# from the LAST line beginning with <tag> (e.g. PERF or PERF_CPU). Parses the
# `key=value key=value ...` format the instrumented binary writes to stderr.
perf_fields() {
  local file="$1" tag="$2"; shift 2
  awk -v tag="^$tag " -v keys="$*" '
    $0 ~ tag {
      delete v
      for (i = 1; i <= NF; i++) { split($i, a, "="); v[a[1]] = a[2] }
      n = split(keys, k, " "); s = ""
      for (j = 1; j <= n; j++) s = s (j > 1 ? " " : "") v[k[j]]
      last = s
    }
    END { print last }
  ' "$file"
}

# report_summary <report.json> -> "succeeded failed retries". Tolerates both the
# top-level-summary and bare-object report shapes.
report_summary() {
  python3 -c 'import json,sys
d = json.load(open(sys.argv[1])); s = d.get("summary", d)
print(s.get("succeeded", 0), s.get("failed", 0), s.get("retries", 0))' "$1"
}
