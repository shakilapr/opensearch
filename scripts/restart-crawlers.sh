#!/usr/bin/env bash
# Restart the four fleet crawlers on the current binary.
#
# Uses the fixed binary (append-delta frontier, no duplicate blob output, busy-retry
# metadata, disk guard) and a higher in-flight bound: the network probe measured p50 0.3 s
# / mean 0.8 s per seed, so 32 in flight left the box at 8% CPU.
set -u
cd /root/projects/search || exit 1
BIN=./target/release/agent0

for i in 0 1 2 3; do
  pkill -f "agent0 crawl /var/lib/search-fleet/work/shard-$i.txt" 2>/dev/null
done
sleep 3
# The duplicate raw spill under the crawl output is no longer written by this binary, so the
# existing copies are dead weight; removing them frees ~45 GB.
for i in 0 1 2 3; do
  rm -rf "raw-dataset/shard-$i/blobs"
done

for i in 0 1 2 3; do
  SEARCH_DATA_DIR=/var/lib/search-fleet/shard-$i \
  SEARCH_CRAWL_ORIGIN_INTERVAL_MS=250 \
  SEARCH_CRAWL_ORIGIN_CAP=250 \
  SEARCH_MIN_FREE_GB=20 \
  nohup timeout 86400 "$BIN" crawl "/var/lib/search-fleet/work/shard-$i.txt" \
    --out "raw-dataset/shard-$i" --max-docs 250000 --max-urls 1000000 \
    --concurrency 256 --compress \
    >> "/var/lib/search-fleet/shard-$i.log" 2>&1 &
  echo "shard-$i started pid=$!"
done
