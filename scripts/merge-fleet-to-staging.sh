#!/usr/bin/env bash
# Merge the four fleet shard indexes (and the current serving index) into ONE staging index,
# using the cheap no-LLM path:
#
#   export <pack> --raw   reads the source index's stored url/title/body and writes a pack.
#                         `--raw` skips the organizer model, so this needs no LLM and no key.
#   import <pack>         feeds each record back through the ingest path into SEARCH_DATA_DIR.
#
# Both steps are resumable: export skips ids already in the pack, import reports records that are
# `unchanged`, so re-running after a kill continues instead of starting over.
#
# The pack is deleted as soon as it has been imported, to bound the disk the merge needs. Nothing
# is deleted from the sources here — the shards and raw data stay put until the staging index has
# been evaluated and swapped in. See docs/OPERATIONS.md and ops/plan.json:deploy-fleet-corpus.
set -uo pipefail
cd /root/projects/search

AGENT0=./target/release/agent0
STAGING=${STAGING:-/var/lib/search-staging}
PACKS=${PACKS:-/var/lib/search-staging-packs}
mkdir -p "$STAGING" "$PACKS"

# name:source-dir. The live serving index is a source too, so the merge keeps what is already served.
SOURCES=(
  "serve:/var/lib/search"
  "shard-3:/var/lib/search-fleet/shard-3"
  "shard-0:/var/lib/search-fleet/shard-0"
  "shard-2:/var/lib/search-fleet/shard-2"
  "shard-1:/var/lib/search-fleet/shard-1"
)

docs() {
  python3 - "$1" <<'PY'
import sqlite3, sys
try:
    c = sqlite3.connect(f"file:{sys.argv[1]}/meta/meta.sqlite?mode=ro", uri=True)
    print(c.execute("SELECT COUNT(*) FROM content").fetchone()[0])
except Exception as e:
    print(f"? {e}")
PY
}

echo "[$(date -Is)] staging merge start -> $STAGING"
for entry in "${SOURCES[@]}"; do
  name=${entry%%:*}; src=${entry#*:}
  pack="$PACKS/$name"
  echo "[$(date -Is)] === $name (source $src) ==="
  if [ ! -d "$src" ]; then echo "[$(date -Is)] SKIP $name: no source dir"; continue; fi
  echo "[$(date -Is)] export --raw -> $pack"
  SEARCH_DATA_DIR="$src" timeout 28800 "$AGENT0" export "$pack" --raw 2>&1 | tail -4
  echo "[$(date -Is)] import -> $STAGING"
  SEARCH_DATA_DIR="$STAGING" timeout 28800 "$AGENT0" import "$pack" 2>&1 | tail -8
  echo "[$(date -Is)] $name done; staging docs = $(docs "$STAGING")"
  # the pack has served its purpose; drop it to keep headroom
  rm -rf "$pack"
  df -h / | tail -1
done
echo "[$(date -Is)] staging merge complete; staging docs = $(docs "$STAGING")"
