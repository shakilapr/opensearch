#!/usr/bin/env python3
"""Measure the crawl's bottleneck stack from the outside, in one pass.

Each stage can only be the binding constraint if the one before it is not. This prints the
evidence for each, in dependency order, so a fix targets the stage that is actually limiting.

Usage: bottleneck_report.py [--window SECONDS]
"""
import argparse
import collections
import json
import os
import subprocess
import sqlite3
import time

FLEET = "/var/lib/search-fleet"
ROOTS = [f"{FLEET}/shard-{i}" for i in range(4)]


def tail_jsonl(path, since, max_bytes=40_000_000):
    """Parse recent JSONL entries without reading a multi-hundred-MB file into memory."""
    try:
        size = os.path.getsize(path)
    except OSError:
        return
    with open(path, errors="ignore") as f:
        f.seek(max(0, size - max_bytes))
        f.readline()
        for line in f:
            line = line.strip()
            if not line.startswith("{"):
                continue
            try:
                d = json.loads(line)
            except Exception:
                continue
            if d.get("fetched_at", 0) >= since:
                yield d


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--window", type=int, default=300)
    args = ap.parse_args()
    now = time.time()
    since = now - args.window
    w = args.window

    print(f"=== BOTTLENECK REPORT over the last {w}s ===\n")

    # ---- Stage 1: are processes alive and producing?
    procs = subprocess.run(["pgrep", "-c", "-f", "agent0 crawl /var/lib"], capture_output=True, text=True)
    print(f"[1] crawler processes: {procs.stdout.strip() or 0}")

    # ---- Stage 2: fetch throughput and outcome mix
    out = collections.Counter()
    hosts = collections.Counter()
    bytes_by = collections.Counter()
    for root in ROOTS:
        for d in tail_jsonl(f"/root/projects/search/raw-dataset/{os.path.basename(root)}/crawl-log.jsonl", since):
            out[d.get("outcome")] += 1
            u = d.get("url") or ""
            if "://" in u:
                hosts[u.split("/")[2]] += 1
            bytes_by[d.get("outcome")] += d.get("bytes") or 0
    total = sum(out.values())
    ing = out.get("ingested", 0)
    if not total:
        print("    no attempts in window")
        return
    print(f"[2] attempts: {total:,} = {total / w * 60:,.0f}/min ({total / w:,.1f}/s)")
    for k, v in out.most_common():
        print(f"      {k:10} {v:7,}  {100 * v / total:5.1f}%")
    print(f"    documents: {ing / w * 3600:,.0f}/h   hosts touched: {len(hosts):,}")

    # ---- Stage 3: slot utilisation (is the fetch bound the constraint?)
    slots = 4 * 256
    print(f"\n[3] configured fetch slots: {slots}  ->  in flight ~= {total / w * 0.8:.0f} (rate x 0.8s latency)")
    print(f"    utilisation: {total / w * 0.8 / slots * 100:.1f}%")
    hot = hosts.most_common(1)[0] if hosts else ("-", 0)
    print(f"    hosts/min: {len(hosts) / w * 60:,.0f}   busiest host: {hot[0]} x{hot[1]}")
    per_host = total / max(len(hosts), 1)
    print(f"    requests per host in window: {per_host:.1f}")
    print(f"    -> fill work comes from: {'frontier, host-diverse' if per_host < 20 else 'a few hosts, rate-limited'}")

    # ---- Stage 4: is discovery supplying new origins fast enough?
    print("\n[4] origin supply (the drain can only fetch what discovery feeds it)")
    for root in ROOTS:
        db = f"{root}/meta/meta.sqlite"
        try:
            c = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
            pend = c.execute(
                "select count(*), count(distinct domain_id) from frontier_entry "
                "where status in ('NEW','READY')"
            ).fetchone()
            c.close()
            print(f"      {os.path.basename(root)}: {pend[1]:>8,} origins / {pend[0]:>10,} URLs pending")
        except Exception as e:
            print(f"      {os.path.basename(root)}: {e}")

    # ---- Stage 5: CPU — is parsing/indexing the constraint?
    print("\n[5] CPU")
    load = os.getloadavg()
    print(f"    load: {load[0]:.2f} {load[1]:.2f} {load[2]:.2f} on {os.cpu_count()} cores")
    ps = subprocess.run(
        ["ps", "-eo", "pcpu,args", "--sort=-pcpu"], capture_output=True, text=True
    ).stdout.splitlines()
    for line in ps[1:6]:
        if "agent0" in line:
            print(f"      {line.strip()[:100]}")

    # ---- Stage 6: resources
    print("\n[6] resources")
    st = os.statvfs("/")
    free_gb = st.f_bavail * st.f_frsize / 1e9
    print(f"    disk free: {free_gb:.0f} GB")
    with open("/proc/self/limits") as f:
        for line in f:
            if line.startswith("Max open files"):
                print(f"    {line.strip()}")
                break
    print(f"    bytes ingested/doc: {bytes_by['ingested'] / max(ing, 1) / 1024:.0f} KB")


if __name__ == "__main__":
    main()
