#!/usr/bin/env python3
"""Stage 2.1 analysis: median (min–max) tables from summary.csv + a stacked
phase-breakdown bar (per mode at a chosen plateau -c, from each run's phases.csv).

Usage: analyze.py [perf-results] [plateau_c]
Prints Markdown tables to stdout; writes perf-results/plots/phase_breakdown.png.
"""
import sys, os, csv, re, statistics, collections
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

ROOT = sys.argv[1] if len(sys.argv) > 1 else "perf-results"
PLATEAU = int(sys.argv[2]) if len(sys.argv) > 2 else 4
MODES = ["scan", "scanverify", "mirror", "mirrorverify"]
LABEL = {"scan": "scan", "scanverify": "scan-verify", "mirror": "mirror", "mirrorverify": "mirror-verify"}
pat = re.compile(r"^(?P<mode>[a-z]+)_C(?P<c>\d+)_r(?P<r>\d+)$")

# mode -> c -> list of rows
rows = collections.defaultdict(lambda: collections.defaultdict(list))
for row in csv.DictReader(open(f"{ROOT}/summary.csv")):
    m = pat.match(row["run_id"])
    if not m or row["exit"] != "0":
        continue
    rows[m["mode"]][int(m["c"])].append(row)

CS = sorted({c for mode in rows for c in rows[mode]})


def med_minmax(vals):
    vals = sorted(vals)
    return statistics.median(vals), min(vals), max(vals)


def table(metric, scale, unit, fmt="{:.0f}"):
    print(f"\n#### {metric} ({unit}) vs `-c` — median (min–max)\n")
    print("| mode | " + " | ".join(f"c={c}" for c in CS) + " |")
    print("|" + "---|" * (len(CS) + 1))
    for mode in MODES:
        cells = []
        for c in CS:
            vs = [float(r[metric]) * scale for r in rows[mode].get(c, [])]
            if not vs:
                cells.append("—"); continue
            md, lo, hi = med_minmax(vs)
            cells.append(f"{fmt.format(md)} ({fmt.format(lo)}–{fmt.format(hi)})")
        print(f"| {LABEL[mode]} | " + " | ".join(cells) + " |")


def best_knee(metric="wall_ms"):
    print(f"\n#### Best `-c` per mode (min median {metric})\n")
    print("| mode | best c | median wall (s) | c=1 median (s) | speedup |")
    print("|---|---|---|---|---|")
    for mode in MODES:
        meds = {c: statistics.median(float(r[metric]) for r in rows[mode][c]) for c in CS if rows[mode].get(c)}
        best = min(meds, key=meds.get)
        s1 = meds.get(1, meds[best])
        print(f"| {LABEL[mode]} | {best} | {meds[best]/1000:.1f} | {s1/1000:.1f} | {s1/meds[best]:.2f}× |")


table("wall_ms", 1 / 1000.0, "wall time, s", "{:.1f}")
table("peak_rss_mb", 1.0, "peak RSS, MB", "{:.0f}")
best_knee()

# ---- phase breakdown stacked bar at PLATEAU c (one sa-perf run per mode) ----
PHASE_ORDER = ["bucket_stream", "xdr_decompress", "xdr_parse_tx", "xdr_parse_result",
               "xdr_parse_ledger", "xdr_parse_scp", "history_fetch", "history_parse",
               "cross_file_verify", "chain_verify", "copy"]
phase_pct = {}  # mode -> {phase: pct}
for mode in MODES:
    f = f"{ROOT}/{mode}_C{PLATEAU}_r1/phases.csv"
    if not os.path.exists(f):
        continue
    d = {row["phase"]: float(row["pct_of_phase_time"]) for row in csv.DictReader(open(f))}
    phase_pct[mode] = d

if phase_pct:
    fig, ax = plt.subplots(figsize=(8, 5))
    modes = [m for m in MODES if m in phase_pct]
    bottoms = [0.0] * len(modes)
    cmap = plt.get_cmap("tab20")
    for i, ph in enumerate(PHASE_ORDER):
        vals = [phase_pct[m].get(ph, 0.0) for m in modes]
        if not any(vals):
            continue
        ax.bar([LABEL[m] for m in modes], vals, bottom=bottoms, label=ph, color=cmap(i % 20))
        bottoms = [b + v for b, v in zip(bottoms, vals)]
    ax.set_ylabel("% of aggregate phase time")
    ax.set_title(f"Phase breakdown at -c={PLATEAU} (self-time %, overlaps wall — see §2.4)")
    ax.legend(loc="center left", bbox_to_anchor=(1, 0.5), fontsize=8)
    plt.savefig(f"{ROOT}/plots/phase_breakdown.png", dpi=120, bbox_inches="tight")
    print(f"\nwrote {ROOT}/plots/phase_breakdown.png")

    print(f"\n#### Phase self-time % at c={PLATEAU} (top phases)\n")
    print("| mode | " + " | ".join(p for p in PHASE_ORDER[:6]) + " |")
    print("|" + "---|" * 7)
    for m in modes:
        print(f"| {LABEL[m]} | " + " | ".join(f"{phase_pct[m].get(p,0):.1f}" for p in PHASE_ORDER[:6]) + " |")
