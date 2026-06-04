#!/usr/bin/env python3
"""Stage-1 (plan §5) pass-criteria checks. Each subcommand prints PASS/FAIL and
exits non-zero on FAIL.

  check_stage1.py inventory <report.json> <src-dir>
      §5.1: report has zero failures and summary.succeeded == file count
      under <src-dir> excluding .well-known.
  check_stage1.py wellknown <mirror-dir>
      §5.2 rewrite rule: .well-known/stellar-history.json is byte-identical
      to the highest history-*.json in the mirror.
  check_stage1.py broken-match <report.json> <manifest.json>
      §5.3(a): a single-section scan report's broken set corresponds to the
      corruption manifest (kind-aware, both directions).
  check_stage1.py repair-match <repair.json> <manifest.json>
      §5.3(d), adapted to the multi-section repair report: file/bucket
      damages are repaired inline during the main pass (not retained in the
      failure sets), so we assert (1) the retry sections end clean, and
      (2) main_pass checkpoint-level detections trace to manifest damages
      and every content-kind damage was detected at its checkpoint (+/-64
      for chain breaks attributed to the adjacent checkpoint).
"""
import json
import re
import sys
from pathlib import Path

CP_FILE_RE = re.compile(r"(history|ledger|transactions|results|scp)-([0-9a-f]{8})\.")


def fail(msg):
    print(f"FAIL: {msg}")
    sys.exit(1)


def ok(msg):
    print(f"PASS: {msg}")
    sys.exit(0)


def load(p):
    with open(p) as f:
        return json.load(f)


def report_is_clean(r):
    return r.get("well_known") is None and not r.get("files") and not r.get("buckets") and not r.get("checkpoints")


def cmd_inventory(report_path, src_dir):
    r = load(report_path)
    if not report_is_clean(r):
        fail(f"report has failures: well_known={r.get('well_known')} files={len(r.get('files', {}))} "
             f"buckets={len(r.get('buckets', []))} checkpoints={len(r.get('checkpoints', []))}")
    succeeded = r["summary"]["succeeded"]
    n = sum(1 for p in Path(src_dir).rglob("*")
            if p.is_file() and ".well-known" not in p.parts)
    if succeeded != n:
        fail(f"succeeded {succeeded} != {n} files under {src_dir} (excl .well-known)")
    ok(f"inventory match: succeeded {succeeded} == {n} files, zero failures")


def cmd_wellknown(mirror_dir):
    m = Path(mirror_dir)
    wk = (m / ".well-known" / "stellar-history.json").read_bytes()
    hist = sorted(m.glob("history/*/*/*/history-*.json"))
    if not hist:
        fail("no history-*.json files in mirror")
    highest = max(hist, key=lambda p: int(CP_FILE_RE.search(p.name).group(2), 16))
    if wk != highest.read_bytes():
        fail(f".well-known differs from highest history file {highest}")
    ok(f".well-known == {highest.relative_to(m)}")


def damage_expectation(key):
    """Map a manifest key to (kind, value) the report must cover."""
    if key.startswith(".well-known"):
        return ("well-known", None)
    if key.startswith("bucket/"):
        return ("bucket", re.search(r"bucket-([0-9a-f]{64})\.", key).group(1))
    m = CP_FILE_RE.search(key)
    if not m:
        raise ValueError(f"unrecognized manifest key: {key}")
    return ("cpfile", (m.group(1), int(m.group(2), 16)))


def cmd_broken_match(report_path, manifest_path):
    r, man = load(report_path), load(manifest_path)
    rep_files = {int(cp): set(v) for cp, v in r.get("files", {}).items()}
    rep_buckets = set(r.get("buckets", []))
    rep_cps = set(r.get("checkpoints", []))
    wk_broken = r.get("well_known") is not None

    # type name used in report 'files' lists for each filename prefix
    type_name = {"history": "history", "ledger": "ledger", "transactions": "transactions",
                 "results": "results", "scp": "scp"}

    problems = []
    # forward: every damage is covered by the report
    man_cps, man_buckets, man_wk = set(), set(), False
    for item in man["items"]:
        kind, key = item["kind"], item["key"]
        dk, val = damage_expectation(key)
        if dk == "well-known":
            man_wk = True
            if not wk_broken:
                problems.append(f"{kind} {key}: report.well_known is null")
        elif dk == "bucket":
            man_buckets.add(val)
            if val not in rep_buckets:
                problems.append(f"{kind} {key}: bucket not in report")
        else:
            ftype, cp = val
            # content corruptions may surface as checkpoint-level failures; a
            # cross-chain break may be attributed to the adjacent checkpoint.
            man_cps.update({cp, cp + 64, cp - 64})
            covered = (ftype in rep_files.get(cp, set())
                       or cp in rep_cps
                       or (kind == "cross-chain" and (cp + 64 in rep_cps or cp - 64 in rep_cps)))
            if not covered:
                problems.append(f"{kind} {key}: cp {cp} ({ftype}) not in report files/checkpoints")

    # reverse: every reported failure traces back to some damage
    for cp in set(rep_files) | rep_cps:
        if cp not in man_cps:
            problems.append(f"report flags cp {cp} but no manifest damage near it")
    for b in rep_buckets - man_buckets:
        problems.append(f"report flags bucket {b} not in manifest")
    if wk_broken and not man_wk:
        problems.append("report flags well-known but manifest has no well-known damage")

    if problems:
        fail("; ".join(problems))
    ok(f"report broken set corresponds to manifest ({len(man['items'])} damages)")


# Corruption kinds that surface as checkpoint-level (cross-file/chain)
# failures in the repair main pass. File-level kinds (delete/truncate/
# byte-flip/invalid-gzip/bucket-*/ledger-header-hash/well-known) are repaired
# inline and leave no trace in the report's failure sets.
CONTENT_KINDS = {"intra-chain", "cross-chain", "txset-hash", "result-hash", "drop-ledger"}


def cmd_repair_match(report_path, manifest_path):
    r, man = load(report_path), load(manifest_path)
    secs = r["sections"]
    problems = []
    for name in ("file_retry", "checkpoint_retry"):
        if name in secs and not report_is_clean(secs[name]):
            problems.append(f"section {name} still has failures")
    main_cps = set(secs.get("main_pass", {}).get("checkpoints", []))

    man_cps = set()
    for item in man["items"]:
        dk, val = damage_expectation(item["key"])
        if dk != "cpfile":
            continue
        cp = val[1]
        man_cps.update({cp - 64, cp, cp + 64})
        if item["kind"] in CONTENT_KINDS and not ({cp - 64, cp, cp + 64} & main_cps):
            problems.append(f"{item['kind']} {item['key']}: cp {cp} not detected in main_pass")
    for cp in main_cps - man_cps:
        problems.append(f"main_pass flags cp {cp} with no manifest damage near it")

    if problems:
        fail("; ".join(problems))
    ok(f"repair report consistent with manifest ({len(man['items'])} damages, "
       f"{len(main_cps)} checkpoint-level detections, retry sections clean)")


if __name__ == "__main__":
    cmd = sys.argv[1]
    if cmd == "inventory":
        cmd_inventory(sys.argv[2], sys.argv[3])
    elif cmd == "wellknown":
        cmd_wellknown(sys.argv[2])
    elif cmd == "broken-match":
        cmd_broken_match(sys.argv[2], sys.argv[3])
    elif cmd == "repair-match":
        cmd_repair_match(sys.argv[2], sys.argv[3])
    else:
        fail(f"unknown subcommand {cmd}")
