#!/usr/bin/env bash
# Try a crawl configuration for a fixed window and report what it achieved.
#
# The point is to change ONE knob at a time against the same seed list, so the effect is
# attributable. Stops its own crawler afterwards and prints the measured rate.
#
# Usage: try-config.sh <label> <seconds> [ENV=VAL ...]
#   e.g. try-config.sh "concurrency-512" 120 SEARCH_CRAWL_CONCURRENCY=512
set -u
LABEL="${1:?label}"; SECS="${2:?seconds}"; shift 2
cd /root/projects/search || exit 1
OUT=/tmp/crawl-trial-$LABEL
LOG=/tmp/crawl-trial-$LABEL.log
rm -rf "$OUT"; mkdir -p "$OUT/data" "$OUT/out"

# A real, host-diverse seed slice: 400 domains from the production list.
head -400 /var/lib/search-fleet/work/shard-1.txt > "$OUT/seeds.txt"

echo "=== $LABEL: ${SECS}s, $* ==="
# Extra KEY=VAL args are environment for the crawler, not commands: `"$@"` alone would try to
# execute them. `env` applies them and then runs the binary.
env SEARCH_DATA_DIR="$OUT/data" \
  SEARCH_CRAWL_ORIGIN_INTERVAL_MS="${SEARCH_CRAWL_ORIGIN_INTERVAL_MS:-250}" \
  SEARCH_CRAWL_ORIGIN_CAP="${SEARCH_CRAWL_ORIGIN_CAP:-250}" \
  "$@" \
  timeout "$SECS" ./target/release/agent0 crawl "$OUT/seeds.txt" \
    --out "$OUT/out" --max-docs 1000000 --max-urls 1000000 \
    --concurrency "${CONCURRENCY:-256}" \
    > "$LOG" 2>&1

python3 - "$OUT" "$SECS" "$LABEL" <<'PY'
import json, sys, collections, os
out, secs, label = sys.argv[1], int(sys.argv[2]), sys.argv[3]
p = os.path.join(out, "out", "crawl-log.jsonl")
c = collections.Counter(); hosts = set()
if os.path.exists(p):
    for line in open(p, errors="ignore"):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            d = json.loads(line)
        except Exception:
            continue
        c[d.get("outcome")] += 1
        u = d.get("url") or ""
        if "://" in u:
            hosts.add(u.split("/")[2])
tot = sum(c.values())
print(f"RESULT {label}: {tot:,} attempts in {secs}s = {tot/secs:,.1f}/s "
      f"({tot/secs*60:,.0f}/min), {c.get('ingested',0):,} docs "
      f"({c.get('ingested',0)/secs*3600:,.0f}/h), {len(hosts):,} hosts")
print("       outcomes:", dict(c.most_common()))
PY
