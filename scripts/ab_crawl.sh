#!/usr/bin/env bash
# A/B the crawl binary: old (full frontier rewrite, duplicate blobs) vs new (delta + no
# duplicate blobs). Same seeds, same shard, same caps, run back to back so the network
# conditions are as close as they can be.
#
# Usage: ab_crawl.sh <binary> <outdir> <seconds>
set -u
BIN="$1"; OUT="$2"; SECS="${3:-150}"
ROOT=/root/projects/search
SEEDS=/var/lib/search-fleet/work/shard-3.txt

rm -rf "$OUT"
mkdir -p "$OUT/data" "$OUT/out"
head -400 "$SEEDS" > "$OUT/seeds.txt"

cd "$ROOT" || exit 1
SEARCH_DATA_DIR="$OUT/data" \
SEARCH_CRAWL_ORIGIN_INTERVAL_MS=250 \
SEARCH_CRAWL_ORIGIN_CAP=250 \
SEARCH_CRAWL_COMPRESS=1 \
  timeout "$SECS" "$BIN" crawl "$OUT/seeds.txt" \
    --out "$OUT/out" --max-docs 100000 --max-urls 200000 --concurrency 64 \
    > "$OUT/run.log" 2>&1

ATT=$(wc -l < "$OUT/out/crawl-log.jsonl" 2>/dev/null || echo 0)
ING=$(grep -c '"outcome":"ingested"' "$OUT/out/crawl-log.jsonl" 2>/dev/null || echo 0)
SNAP=$(stat -c %s "$OUT/out/frontier.json" 2>/dev/null || echo 0)
ADDS=$(stat -c %s "$OUT/out/frontier.adds" 2>/dev/null || echo 0)
BLOBS=$(du -sm "$OUT/out/blobs" 2>/dev/null | cut -f1 || echo 0)
DATABLOBS=$(du -sm "$OUT/data/blobs" 2>/dev/null | cut -f1 || echo 0)
SUM=$(tail -1 "$OUT/run.log" | grep -o '"ingested":[0-9]*' || true)

echo "attempts=$ATT ingested=$ING frontier_snapshot=${SNAP}B adds=${ADDS}B out_blobs=${BLOBS}MB data_blobs=${DATABLOBS}MB $SUM"
