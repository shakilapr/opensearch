#!/usr/bin/env bash
# Build dense vectors for every fleet shard, one shard at a time.
#
# Why serial and CPU-pinned. Four concurrent embed jobs took the box to load 38.8 on 16 cores
# (measured 2026-10-29), and four crawlers share those same cores — the crawlers are the source of
# the text being embedded, so starving them to embed faster is self-defeating. One shard at a time
# with `AllocatedCPU=6` leaves the crawlers their share and still finishes overnight.
#
# Measured single-shard rate on this host: 13.2 docs/s at 2000 chars, 21.7 at 300. The fleet is
# ~1.35M documents, so this is roughly 19 hours — a background job, not a blocking step.
set -uo pipefail

cd /root/projects/search || exit 1
BIN=./target/release/agent0
CACHE=/models/fastembed
LOG=/var/lib/search-fleet/embed-build.log
: > "$LOG"

for shard in 0 1 2 3; do
    dir="/var/lib/search-fleet/shard-$shard"
    out="$dir/vectors.bin"
    [ -d "$dir/meta" ] || { echo "skip $dir (no metadata)" | tee -a "$LOG"; continue; }
    if [ -f "$out" ] && [ "$(stat -c%s "$out")" -gt 1000000 ]; then
        echo "skip shard-$shard (vectors.bin already present)" | tee -a "$LOG"
        continue
    fi
    echo "=== shard-$shard $(date +%H:%M:%S) ===" | tee -a "$LOG"
    # Read-only index access: the crawler holds the Tantivy writer lock, so the embed pass must not
    # try to take it. `embed build` already opens read-only.
    SEARCH_DATA_DIR="$dir" SEARCH_MODEL_CACHE="$CACHE" \
        "$BIN" embed build --out "$out" 2>&1 | tee -a "$LOG"
done

echo "=== done $(date +%H:%M:%S) ===" | tee -a "$LOG"
for shard in 0 1 2 3; do
    f="/var/lib/search-fleet/shard-$shard/vectors.bin"
    if [ -f "$f" ]; then
        echo "  shard-$shard: $(du -h "$f" | cut -f1)" | tee -a "$LOG"
    else
        echo "  shard-$shard: absent" | tee -a "$LOG"
    fi
done
