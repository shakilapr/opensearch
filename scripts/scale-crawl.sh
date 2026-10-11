#!/usr/bin/env bash
# Scale the crawl across shards and processes.
#
# Why shards and not one big run: Tantivy's writer lock is exclusive per index directory, so two
# crawls cannot share one data root — the second dies with LockBusy. Each shard therefore owns its
# own data root (index + metadata) and its own raw output, and they run in parallel without
# contention. The shard indexes are independent; `agent0 export` per shard + `agent0 import` merges
# them through the portable pack when a single searchable index is wanted.
#
# Why seeds are split by line: the crawler follows links within a host, so two shards holding
# different hosts do not overlap — no URL is fetched twice across the fleet.
#
# Usage:
#   scripts/scale-crawl.sh [shards] [max_docs_per_shard] [concurrency] [seeds_file]
# Defaults: 8 shards, 200000 docs each, concurrency 512, seeds built from Tranco top-1M.
set -uo pipefail

SHARDS="${1:-8}"
MAX_DOCS="${2:-200000}"
# Cross-origin in-flight fetches per shard: 512 by default (or SEARCH_CRAWL_CONCURRENCY)
# so a 16-core box's network + parse work is actually overlapped. At the old 64 the
# fleet sat at ~14% CPU / ~14.6% RAM against an 80% target with ~27 s mean fetch
# latency: the aggregate global fetch bound was the binding constraint, not the
# per-origin politeness floor (which is unchanged — one request in flight per host
# plus ORIGIN_INTERVAL_MS). More slots mean more distinct origins resolving, parsing
# and indexing at once, which is what raises CPU/RAM without hammering any host.
CONCURRENCY="${3:-${SEARCH_CRAWL_CONCURRENCY:-512}}"
SEEDS="${4:-}"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO/target/release/agent0"
FLEET="${FLEET_ROOT:-/var/lib/search-fleet}"
RAW="${RAW_ROOT:-$REPO/raw-dataset}"
WORK="$FLEET/work"
LOGS="$FLEET/logs"

mkdir -p "$WORK" "$LOGS" "$FLEET"

# Seed list: build from Tranco top-1M when none is given. Ranks are dropped (the crawler does not
# order by them), and each host becomes an https root — the seed layer canonicalises from there.
if [ -z "$SEEDS" ]; then
  SEEDS="$WORK/seeds-all.txt"
  if [ ! -s "$SEEDS" ]; then
    echo "== building the seed list from Tranco top-1M =="
    curl -sSL --max-time 300 -o "$WORK/tranco.zip" https://tranco-list.eu/top-1m.csv.zip
    ( cd "$WORK" && unzip -o -q tranco.zip )
    # top-1m.csv: rank,domain
    awk -F, '{print "https://" $2}' "$WORK/top-1m.csv" > "$SEEDS"
  fi
fi
TOTAL=$(wc -l < "$SEEDS")
echo "seeds: $TOTAL hosts from $SEEDS"

if [ ! -x "$BIN" ]; then
  echo "building agent0 (release)…"
  ( cd "$REPO" && cargo build --release -p search-agent0 )
fi

# Split round-robin so each shard gets a spread of the ranking rather than a contiguous block.
for k in $(seq 0 $((SHARDS - 1))); do
  awk -v k="$k" -v n="$SHARDS" 'NR % n == k' "$SEEDS" > "$WORK/shard-$k.txt"
  echo "  shard $k: $(wc -l < "$WORK/shard-$k.txt") seeds -> data root $FLEET/shard-$k, raw $RAW/shard-$k"
done

# One worker per shard. The worker restarts its crawl if it exits (a crash, a transient network
# failure) until the shard reaches its document budget or runs out of frontier — and $FLEET/stop
# ends the fleet cleanly.
worker() {
  local k="$1"
  mkdir -p "$FLEET/shard-$k" "$RAW/shard-$k"
  while [ ! -f "$FLEET/stop" ]; do
    # NOTE: the seeds file is POSITIONAL (`agent0 crawl <seedsfile>`), not a `--seeds` flag — the
    # first attempt here passed `--seeds FILE`, which the CLI read as a filename called "--seeds".
    SEARCH_DATA_DIR="$FLEET/shard-$k" \
    SEARCH_CRAWL_ORIGIN_INTERVAL_MS="${ORIGIN_INTERVAL_MS:-250}" \
    timeout 21600 "$BIN" crawl "$WORK/shard-$k.txt" \
      --out "$RAW/shard-$k" \
      --max-docs "$MAX_DOCS" \
      --max-urls "$((MAX_DOCS * 4))" \
      --concurrency "$CONCURRENCY" \
      --compress >> "$LOGS/shard-$k.log" 2>&1
    echo "$(date -Is) shard $k run ended (exit $?)" >> "$LOGS/shard-$k.log"
    # A finished shard (frontier drained) should not spin: pause before the next pass.
    sleep "${RESTART_PAUSE_S:-30}"
  done
}

echo "== launching $SHARDS shard workers (max $MAX_DOCS docs each, concurrency $CONCURRENCY) =="
pids=()
for k in $(seq 0 $((SHARDS - 1))); do
  worker "$k" &
  pids+=($!)
done

trap 'touch "$FLEET/stop"; wait; exit 0' INT TERM
wait
