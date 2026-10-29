#!/usr/bin/env bash
# Tag every fleet shard into the concept matrix, one shard at a time.
#
# Serial on purpose: each shard has its own SQLite database, but four concurrent tagging passes
# would compete with four crawlers for the same cores and the same disks, and the crawlers are the
# source of the text being tagged. One shard at a time keeps the crawl at its measured rate while
# the matrix fills.
#
# Not `set -e`: a shard that fails should not abort the other three, because each is independent and
# a partially-filled matrix is still usable.
set -uo pipefail

cd /root/projects/search || exit 1
BIN=./target/release/agent0
LOG=/var/lib/search-fleet/concepts-build.log
: > "$LOG"

for shard in 0 1 2 3; do
    dir="/var/lib/search-fleet/shard-$shard"
    [ -d "$dir/meta" ] || { echo "skip $dir (no metadata)" | tee -a "$LOG"; continue; }
    # A crawler holds the writer handle; the tagging pass only needs to write in short batches and
    # the metadata layer already retries SQLITE_BUSY, so no crawl stop is required.
    echo "=== shard-$shard $(date +%H:%M:%S) ===" | tee -a "$LOG"
    SEARCH_DATA_DIR="$dir" "$BIN" concepts build 2>&1 | tee -a "$LOG"
done

echo "=== done $(date +%H:%M:%S) ===" | tee -a "$LOG"
for shard in 0 1 2 3; do
    python3 - "$shard" <<'PY' | tee -a "$LOG"
import sqlite3, sys
s = sys.argv[1]
c = sqlite3.connect(f'file:/var/lib/search-fleet/shard-{s}/meta/meta.sqlite?mode=ro', uri=True)
a = c.execute('select count(*) from doc_concept').fetchone()[0]
d = c.execute('select count(*) from doc_concept_dim').fetchone()[0]
n = c.execute('select count(distinct resource_id) from doc_concept').fetchone()[0]
t = c.execute('select count(*) from document_version').fetchone()[0]
print(f'  shard-{s}: {a:,} associations, {d:,} dimension rows, {n:,}/{t:,} docs tagged')
PY
done
