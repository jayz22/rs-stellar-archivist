#!/usr/bin/env python3
"""Plot the production verify scaling graph from perf-results/scaling_graph.csv.

Two panels:
  (1) Speed vs cores: throughput (MB/s, left) with an ideal-linear reference,
      and wall-clock time (s, right).
  (2) Utilization vs cores: cores actually used (getrusage CPU/wall), with a
      y=x perfect-use reference.

Usage: plot_scaling_graph.py [perf-results/scaling_graph.csv]
"""
import sys, os, csv
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

path = sys.argv[1] if len(sys.argv) > 1 else "perf-results/scaling_graph.csv"
os.makedirs("perf-results/plots", exist_ok=True)

# One production series: collect rows by core cap (ignore the internal label column).
rows = {}
for row in csv.DictReader(open(path)):
    if row.get("failed") not in ("0", "", None):
        continue
    rows[int(row["cores"])] = row

xs = sorted(rows)
mbps = [float(rows[c]["mb_per_s"]) for c in xs]
wall = [float(rows[c]["wall_s"]) for c in xs]
used = [float(rows[c]["cores_avg"]) for c in xs]

# Colors: left-axis series and right-axis series get distinct hues, axis labels tinted to match.
C_THRU, C_WALL = "#1f77b4", "#d62728"
C_USED = "#2ca02c"

fig, (ax1, ax2) = plt.subplots(1, 2, figsize=(13, 5.2))

# --- Panel 1: throughput (left) + wall time (right) ---
ax1.plot(xs, mbps, marker="o", color=C_THRU, label="throughput (MB/s)")
if xs:
    base_c, base_mbps = xs[0], mbps[0]
    ideal = [base_mbps * c / base_c for c in xs]
    ax1.plot(xs, ideal, "--", color=C_THRU, alpha=0.45, label="ideal linear")
ax1.set_xlabel("cores available (taskset cap)")
ax1.set_ylabel("verify throughput (MB/s)", color=C_THRU)
ax1.tick_params(axis="y", labelcolor=C_THRU)
ax1.set_ylim(bottom=0)
ax1.grid(True, alpha=0.3)

ax1b = ax1.twinx()
ax1b.plot(xs, wall, marker="s", color=C_WALL, label="wall time (s)")
ax1b.set_ylabel("wall-clock time (s)", color=C_WALL)
ax1b.tick_params(axis="y", labelcolor=C_WALL)
ax1b.set_ylim(bottom=0)
ax1.set_title("Throughput & wall time vs cores")
l1, lab1 = ax1.get_legend_handles_labels()
l2, lab2 = ax1b.get_legend_handles_labels()
ax1.legend(l1 + l2, lab1 + lab2, loc="center right")

# --- Panel 2: cores used (y=x perfect-use ref) ---
ax2.plot(xs, used, marker="o", color=C_USED, label="cores used (cores_avg)")
hi = max(xs) if xs else 32
ax2.plot([0, hi], [0, hi], ":", color="gray", alpha=0.6, label="y=x (perfect use)")
ax2.set_xlabel("cores available (taskset cap)")
ax2.set_ylabel("process-wide cores used", color=C_USED)
ax2.tick_params(axis="y", labelcolor=C_USED)
ax2.set_ylim(bottom=0)
ax2.grid(True, alpha=0.3)
ax2.set_title("Core utilization vs cores")
ax2.legend(loc="center right")

fig.suptitle(
    "stellar-archivist verify scaling — 65,536 checkpoints, fs:// pubnet mirror, 32-core aarch64",
    fontsize=12,
)
out = "perf-results/plots/scaling_graph.png"
plt.tight_layout(rect=[0, 0, 1, 0.96])
plt.savefig(out, dpi=120, bbox_inches="tight")
plt.close()
print("wrote", out)
