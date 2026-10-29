#!/usr/bin/env bash
# Index one shard's already-downloaded documents, safely.
#
# Two hard-won constraints:
#   1. The crawler holds the Tantivy writer lock, so it must be stopped for the build.
#   2. A build run inside the Hermes worker cgroup is OOM-killed at ~4 GB regardless of the box's
#      free memory (measured twice on shard-1: CONSTRAINT_MEMCG kill at 3.5-3.8 GB). systemd-run
#      puts it in its own unit with an explicit cap instead.
#
# Usage: index-one-shard.sh <shard-index>
set -u
SHARD="${1:?usage: index-one-shard.sh <0|1|2|3>}"
cd /root/projects/search || exit 1
ROOT=/var/lib/search-fleet/shard-$SHARD
LOG=/var/lib/search-fleet/index-build.log
BIN=./target/release/agent0

echo "=== shard-$SHARD: stopping crawler at $(date -Is)" | tee -a "$LOG"
pkill -f "agent0 crawl /var/lib/search-fleet/work/shard-$SHARD.txt" 2>/dev/null
for _ in $(seq 1 40); do
  pgrep -f "agent0 crawl /var/lib/search-fleet/work/shard-$SHARD.txt" >/dev/null || break
  sleep 1
done

# A rebuild replaces the index rather than merging into it. `delete_term` on a document id does
# not remove an identical id from another segment reliably, so re-indexing into a populated index
# accumulates duplicates: measured 2026-10-28, 1.74M documents for 1.17M unique inputs. The index
# is derived data — the corpus (resource/document_version/content + text files) is the source of
# truth — so moving it aside is safe and is the only way to get an exact count.
if [ "${KEEP_INDEX:-0}" != "1" ]; then
  echo "=== shard-$SHARD: moving the old index aside at $(date -Is)" | tee -a "$LOG"
  rm -rf "$ROOT/tantivy.prev"
  mv "$ROOT/tantivy" "$ROOT/tantivy.prev"
fi

echo "=== shard-$SHARD: indexing at $(date -Is)" | tee -a "$LOG"
systemd-run --unit="idxbuild-s$SHARD" --collect --wait \
  --property=MemoryMax=16G --property=MemorySwapMax=4G \
  /bin/bash -c "cd /root/projects/search && $BIN index build --dir $ROOT --batch 512" \
  2>&1 | grep -vE "Meta file|Deleted \"|Prepared commit|committing|save metas|Running garbage|Garbage collect|Starting merge|Merging|Buffer limit" \
  | tee -a "$LOG"

echo "=== shard-$SHARD: restarting crawler at $(date -Is)" | tee -a "$LOG"
SEARCH_DATA_DIR="$ROOT" \
SEARCH_CRAWL_ORIGIN_INTERVAL_MS=250 \
SEARCH_CRAWL_ORIGIN_CAP=250 \
SEARCH_MIN_FREE_GB=20 \
nohup timeout 86400 "$BIN" crawl "/var/lib/search-fleet/work/shard-$SHARD.txt" \
  --out "raw-dataset/shard-$SHARD" --max-docs 250000 --max-urls 1000000 \
  --concurrency 256 --compress \
  >> "/var/lib/search-fleet/shard-$SHARD.log" 2>&1 &
