#!/usr/bin/env bash
# Index the documents already downloaded on every fleet shard.
#
# The corpus is on disk (1.16M document_version rows with extracted text) but only ~569k are in
# Tantivy. `agent0 index build` closes that gap from local text, with no network I/O. Measured
# 1,522 docs/s, so the whole fleet is ~13 minutes.
#
# A shard's crawler must be stopped for the duration: `index build` takes the Tantivy writer, and
# the crawler holds it. This does them one at a time and restarts that shard's crawler immediately
# afterwards, so at most one of four shards is not crawling at any moment.
set -u
cd /root/projects/search || exit 1
BIN=./target/release/agent0
LOG=/var/lib/search-fleet/index-build.log

for i in 0 1 2 3; do
  ROOT=/var/lib/search-fleet/shard-$i
  echo "=== shard-$i: stopping crawler" | tee -a "$LOG"
  pkill -f "agent0 crawl /var/lib/search-fleet/work/shard-$i.txt" 2>/dev/null
  # Wait for the writer lock to be released (the process exits, then the lock file goes).
  for _ in $(seq 1 30); do
    pgrep -f "agent0 crawl /var/lib/search-fleet/work/shard-$i.txt" >/dev/null || break
    sleep 1
  done

  echo "=== shard-$i: indexing at $(date -Is)" | tee -a "$LOG"
  "$BIN" index build --dir "$ROOT" --batch 512 2>&1 \
    | grep -vE "Meta file|Deleted \"|Prepared commit|committing|save metas|Running garbage|Garbage collect|Starting merge|Merging" \
    | tee -a "$LOG"

  echo "=== shard-$i: restarting crawler" | tee -a "$LOG"
  SEARCH_DATA_DIR="$ROOT" \
  SEARCH_CRAWL_ORIGIN_INTERVAL_MS=250 \
  SEARCH_CRAWL_ORIGIN_CAP=250 \
  SEARCH_MIN_FREE_GB=20 \
  nohup timeout 86400 "$BIN" crawl "/var/lib/search-fleet/work/shard-$i.txt" \
    --out "raw-dataset/shard-$i" --max-docs 250000 --max-urls 1000000 \
    --concurrency 256 --compress \
    >> "/var/lib/search-fleet/shard-$i.log" 2>&1 &
done
echo "=== done at $(date -Is)" | tee -a "$LOG"
