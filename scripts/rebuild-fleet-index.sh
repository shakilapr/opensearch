#!/usr/bin/env bash
# Rebuild every index onto the excerpt-only model (v2 architecture, index model IM-3).
#
# Why one script: a schema edit makes the existing Tantivy directories unopenable — the
# `_schema_hash` guard refuses them, correctly. So "change the schema" and "rebuild the index" are
# one operation, not two, and splitting them leaves the fleet unreadable in between (this is what
# forced the revert in 8655408).
#
# Order inside the script is stop writers -> rebuild -> restart writers, per root. The old
# directories are kept as `tantivy.fulltext-<ts>`: the metadata DB and `text/` are the source of
# truth, so a rebuild is repeatable, and the old copy is a rollback path until the gates pass.
#
# Gates (printed per root, and to be checked against metadata counts):
#   * indexed == document_version rows with newest version AND a text file (missing_text == 0)
#   * tantivy size per document falls by ~an order of magnitude
#   * `agent0 serve` opens it and /search returns results with snippets
#
# Run it under systemd-run, never in the agent shell: a normal tool-call process is OOM-killed at
# ~4 GB by CONSTRAINT_MEMCG even with 60 GB free.
set -uo pipefail
cd /root/projects/search || exit 1

BIN=./target/release/agent0
TS=$(date +%Y%m%d-%H%M)
ROOTS=(
  /var/lib/search-fleet/shard-0
  /var/lib/search-fleet/shard-1
  /var/lib/search-fleet/shard-2
  /var/lib/search-fleet/shard-3
  /var/lib/search
)
LOG=/var/lib/search-fleet/rebuild-index-$TS.log

log() { printf '%s %s\n' "$(date -Is)" "$*" | tee -a "$LOG"; }

stop_crawlers() {
  for i in 0 1 2 3; do
    # The pattern names the seed file, so it cannot match this script's own shell.
    pkill -f "agent0 crawl /var/lib/search-fleet/work/shard-$i.txt" 2>/dev/null
  done
  sleep 3
  local left
  left=$(pgrep -fc "agent0 crawl /var/lib/search-fleet/work/shard-" 2>/dev/null || true)
  log "crawlers stopped (remaining=$((${left:-0})))"
}

start_crawlers() {
  for i in 0 1 2 3; do
    SEARCH_DATA_DIR="/var/lib/search-fleet/shard-$i" \
    SEARCH_CRAWL_ORIGIN_INTERVAL_MS=250 \
    SEARCH_CRAWL_ORIGIN_CAP=250 \
    SEARCH_MIN_FREE_GB=20 \
    nohup timeout 86400 "$BIN" crawl "/var/lib/search-fleet/work/shard-$i.txt" \
      --out "raw-dataset/shard-$i" --max-docs 250000 --max-urls 1000000 \
      --concurrency 32 --compress \
      >> "/var/lib/search-fleet/shard-$i.log" 2>&1 &
    log "shard-$i crawler started pid=$!"
  done
}

log "rebuild start: ts=$TS binary=$($BIN --version 2>/dev/null || echo '?')"

systemctl stop search-api
stop_crawlers

for root in "${ROOTS[@]}"; do
  if [ ! -d "$root/meta" ]; then
    log "SKIP $root (no metadata DB)"
    continue
  fi
  if [ -d "$root/tantivy" ]; then
    mv "$root/tantivy" "$root/tantivy.fulltext-$TS"
  fi
  mkdir -p "$root/tantivy"
  log "building $root"
  SEARCH_DATA_DIR="$root" "$BIN" index build --dir "$root" 2>&1 | tail -2 | tee -a "$LOG"
  log "built $root: $(du -sm "$root/tantivy" | cut -f1) MB tantivy"
done

start_crawlers
systemctl start search-api
sleep 5
log "search-api: $(systemctl is-active search-api)"
log "rebuild done"