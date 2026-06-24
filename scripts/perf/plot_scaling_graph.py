#!/usr/bin/env python3
"""Plot the verify scaling graph from perf-results/scaling_graph.csv.
Two panels: (1) throughput (MB/s) vs cores available, with an ideal-linear
reference for the winner; (2) process-wide cores actually used vs cores available.
Usage: plot_scaling_graph.py [perf-results/scaling_graph.csv]"""
import sys, os, csv, collections
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

path = sys.argv[1] if len(sys.argv) > 1 else "perf-results/scaling_graph.csv"
os.makedirs("perf-results/plots", exist_ok=True)

data = collections.defaultdict(dict)  # label -> cores -> row
for row in csv.DictReader(open(path)):
    if row.get("failed") not in ("0", "", None):
        continue
    data[row["label"]][int(row["cores"])] = row

fig, (ax1, ax2) = plt.subplots(1, 2, figsize=(12, 5))

for label in sorted(data):
    xs = sorted(data[label])
    mbps = [float(data[label][c]["mb_per_s"]) for c in xs]
    used = [float(data[label][c]["cores_avg"]) for c in xs]
    ax1.plot(xs, mbps, marker="o", label=label)
    ax2.plot(xs, used, marker="o", label=label)

# Ideal-linear reference anchored at the winner's lowest-core point.
if "A" in data:
    xs = sorted(data["A"])
    base_c = xs[0]
    base_mbps = float(data["A"][base_c]["mb_per_s"])
    ideal = [base_mbps * c / base_c for c in xs]
    ax1.plot(xs, ideal, "k--", alpha=0.5, label="A ideal-linear")

ax1.set_xlabel("cores available (taskset cap)")
ax1.set_ylabel("verify throughput (MB/s)")
ax1.set_title("Throughput vs cores — winner (A) scales, base is flat")
ax1.grid(True, alpha=0.3); ax1.legend()

ax2.plot([0, 32], [0, 32], "k:", alpha=0.4, label="y=x (perfect use)")
ax2.set_xlabel("cores available (taskset cap)")
ax2.set_ylabel("process-wide cores used")
ax2.set_title("Core utilization (getrusage, all threads)")
ax2.grid(True, alpha=0.3); ax2.legend()

out = "perf-results/plots/scaling_graph.png"
plt.tight_layout(); plt.savefig(out, dpi=120, bbox_inches="tight"); plt.close()
print("wrote", out)
