#!/usr/bin/env bash
# Restart the crawler for one shard, if it is not already running.
#
# Needed because the indexing scripts stop a shard's crawler and restart it afterwards; a kill
# between those two steps leaves that shard idle with nothing supervising it (observed 2026-10-29:
# shards 2 and 3 both stopped, and only the per-shard check found it).
#
# Usage: start-crawler.sh <shard-index>
set -u
SHARD="${1:?usage: start-crawler.sh <0|1|2|3>}"
cd /root/projects/search || exit 1
ROOT=/var/lib/search-fleet/shard-$SHARD

if pgrep -f "agent0 crawl /var/lib/search-fleet/work/shard-$SHARD.txt" >/dev/null; then
  echo "shard-$SHARD: already running"
  exit 0
fi

SEARCH_DATA_DIR="$ROOT" \
SEARCH_CRAWL_ORIGIN_INTERVAL_MS=250 \
SEARCH_CRAWL_ORIGIN_CAP=250 \
SEARCH_MIN_FREE_GB=20 \
nohup timeout 86400 ./target/release/agent0 crawl "/var/lib/search-fleet/work/shard-$SHARD.txt" \
  --out "raw-dataset/shard-$SHARD" --max-docs 250000 --max-urls 1000000 \
  --concurrency 256 --compress \
  >> "/var/lib/search-fleet/shard-$SHARD.log" 2>&1 &
echo "shard-$SHARD: started pid=$!"
