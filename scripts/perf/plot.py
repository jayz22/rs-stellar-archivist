#!/usr/bin/env python3
"""Plot scaling results. Usage: plot.py [perf-results/summary.csv]
Parses run_id of the form <mode>_C<n>_r<rep>; plots wall-time and peak-RSS vs -C
(min across reps), one line per mode."""
import sys, os, csv, re, collections
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

path = sys.argv[1] if len(sys.argv) > 1 else "perf-results/summary.csv"
os.makedirs("perf-results/plots", exist_ok=True)
pat = re.compile(r"^(?P<mode>[a-z]+)_C(?P<c>\d+)_r\d+$")
data = collections.defaultdict(lambda: collections.defaultdict(list))  # mode -> C -> [(wall_ms, rss_mb)]
for row in csv.DictReader(open(path)):
    m = pat.match(row["run_id"])
    if not m or row.get("exit", "0") not in ("0", ""):
        continue
    try:
        wall = float(row["wall_ms"])
        rss = float(row["peak_rss_mb"] or row["peak_rss_mb_os"] or 0)
    except ValueError:
        continue
    data[m["mode"]][int(m["c"])].append((wall, rss))

def plot(idx, ylabel, fname, scale=1.0):
    plt.figure()
    for mode in sorted(data):
        xs = sorted(data[mode])
        ys = [min(v[idx] for v in data[mode][c]) * scale for c in xs]
        plt.plot(xs, ys, marker="o", label=mode)
    plt.xlabel("concurrency (-C)"); plt.ylabel(ylabel)
    plt.xscale("log", base=2); plt.legend(); plt.grid(True, which="both", alpha=0.3)
    plt.savefig(f"perf-results/plots/{fname}", dpi=120, bbox_inches="tight"); plt.close()

if data:
    plot(0, "wall time (s)", "time_vs_concurrency.png", 1/1000.0)
    plot(1, "peak RSS (MB)", "rss_vs_concurrency.png")
    print("wrote perf-results/plots/{time,rss}_vs_concurrency.png")
else:
    print("no parseable rows in", path)
