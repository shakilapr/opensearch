#!/usr/bin/env bash
# Start the crawler for one shard, if it is not already running.
#
# Needed because the indexing scripts stop a shard's crawler and restart it afterwards; a kill
# between those two steps leaves that shard idle with nothing supervising it (observed 2026-10-29:
# shards 2 and 3 both stopped, and only the per-shard check found it).
#
# The crawler runs as its **own transient systemd unit**, not as a `nohup ... &` child of the
# caller. That detail is not cosmetic: the rebuild script runs under `systemd-run --wait`, and when
# that unit exits systemd kills its whole cgroup — taking every child with it. Measured 2026-11-02:
# four crawlers "started" by the rebuild were dead minutes later, every shard idle, and the only
# evidence was an empty process list. A unit of its own outlives whatever started it.
#
# Usage: start-crawler.sh <shard-index>
set -u
SHARD="${1:?usage: start-crawler.sh <0|1|2|3>}"
cd /root/projects/search || exit 1
ROOT=/var/lib/search-fleet/shard-$SHARD
UNIT="crawler-shard-$SHARD"
LOG="/var/lib/search-fleet/shard-$SHARD.log"

if pgrep -f "agent0 crawl /var/lib/search-fleet/work/shard-$SHARD.txt" >/dev/null; then
  echo "shard-$SHARD: already running"
  exit 0
fi
if systemctl is-active --quiet "$UNIT"; then
  echo "shard-$SHARD: unit $UNIT already active"
  exit 0
fi
systemctl reset-failed "$UNIT" 2>/dev/null || true

# CPUQuota leaves headroom: four crawlers at 250% each is at most 10 of 16 cores, so the box keeps
# ~6 cores for serving, batch passes and anything else. MemoryMax keeps a crawl from OOM-killing a
# neighbour (a 4 GB cgroup kill took a rebuild down twice).
systemd-run --unit="$UNIT" --collect \
  --property=WorkingDirectory=/root/projects/search \
  --property=CPUQuota="${CRAWLER_CPU_QUOTA:-250%}" \
  --property=MemoryMax="${CRAWLER_MEMORY_MAX:-6G}" \
  --property=Environment=SEARCH_DATA_DIR="$ROOT" \
  --property=Environment=SEARCH_CRAWL_ORIGIN_INTERVAL_MS=250 \
  --property=Environment=SEARCH_CRAWL_ORIGIN_CAP=250 \
  --property=Environment=SEARCH_CRAWL_REFILL_BATCH="${CRAWLER_REFILL_BATCH:-4096}" \
  --property=Environment=SEARCH_CRAWL_PER_ORIGIN_SHARE="${CRAWLER_PER_ORIGIN_SHARE:-8}" \
  --property=Environment=SEARCH_MIN_FREE_GB=20 \
  /bin/bash -c "exec timeout 86400 ./target/release/agent0 crawl /var/lib/search-fleet/work/shard-$SHARD.txt --out raw-dataset/shard-$SHARD --max-docs 250000 --max-urls 1000000 --concurrency 256 --compress >> $LOG 2>&1" \
  >/dev/null
echo "shard-$SHARD: unit $UNIT started"