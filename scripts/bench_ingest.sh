#!/usr/bin/env bash
# Reproducible ingest benchmark. Every claim about "docs/second" in the work plan must come from
# this script, on a fixed pack, with the same flags — otherwise the numbers are not comparable.
#
# It measures what the ingest path actually costs:
#   * docs/second          — wall-clock over a fixed pack
#   * bytes written / doc  — from /proc/<pid>/io, i.e. what the disk really did, not what the index kept
#   * commit + publish     — parsed from the run's own log lines
#   * segments             — base + overlay segment count from the opened index
#
# Usage:
#   scripts/bench_ingest.sh                 # 50,000 docs from shard-2 (default)
#   scripts/bench_ingest.sh --source shard-1 --limit 20000
#   scripts/bench_ingest.sh --batch 1024
#
# The benchmark never touches /var/lib/search. It uses a scratch index under the cache dir.
set -uo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
AGENT0="$REPO/target/release/agent0"
SCRATCH="${SCRATCH:-$HOME/.hermes/profiles/search/cache/scratch}"
SOURCE="shard-2"
LIMIT=50000
BATCH=""
PACK="$SCRATCH/bench-pack"
INDEX="$SCRATCH/bench-index"
LOG="$SCRATCH/bench-ingest.log"

while [ $# -gt 0 ]; do
  case "$1" in
    --source) SOURCE="$2"; shift 2 ;;
    --limit) LIMIT="$2"; shift 2 ;;
    --batch) BATCH="$2"; shift 2 ;;
    --pack) PACK="$2"; shift 2 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

SRC_DIR="/var/lib/search-fleet/$SOURCE"
if [ ! -d "$SRC_DIR/meta" ]; then
  echo "no such source index: $SRC_DIR" >&2
  exit 2
fi
mkdir -p "$SCRATCH"

echo "== bench: source=$SOURCE limit=$LIMIT batch=${BATCH:-default} =="
rm -rf "$INDEX"
[ -d "$PACK" ] || mkdir -p "$PACK"

# --- export (the cheap half; included so the pack is always fresh and comparable) ---------------
t0=$(date +%s.%N)
SEARCH_DATA_DIR="$SRC_DIR" "$AGENT0" export "$PACK" --raw --limit "$LIMIT" >"$SCRATCH/bench-export.json" 2>"$SCRATCH/bench-export.log"
t1=$(date +%s.%N)
RECORDS=$(python3 - "$SCRATCH/bench-export.json" <<'PY'
import json, sys
# agent0 writes its tracing log to stdout, so the summary JSON is the last brace-delimited blob.
txt = open(sys.argv[1], encoding="utf-8", errors="replace").read()
i = txt.find("\n{\n")
if i < 0:
    i = txt.find("{\n")
try:
    print(json.loads(txt[i:])["records"])
except Exception:
    print(0)
PY
)
echo "export:   $RECORDS records in $(python3 -c "print(f'{$t1-$t0:.2f}')")s"

# --- import (the measured half) ----------------------------------------------------------------
export SEARCH_DATA_DIR="$INDEX"
if [ -n "$BATCH" ]; then export SEARCH_BULK_BATCH="$BATCH"; fi
"$AGENT0" import "$PACK" >"$LOG" 2>&1 &
PID=$!
t0=$(date +%s.%N)
# Sample the process I/O counters while it runs.
PEAK_WRITE=0
while kill -0 "$PID" 2>/dev/null; do
  if [ -r "/proc/$PID/io" ]; then
    w=$(awk '/^write_bytes:/{print $2}' "/proc/$PID/io" 2>/dev/null || echo 0)
    [ "${w:-0}" -gt "$PEAK_WRITE" ] && PEAK_WRITE=$w
  fi
  sleep 1
done
wait "$PID"; RC=$?
t1=$(date +%s.%N)

if [ "$RECORDS" -gt 0 ]; then
  python3 - "$t0" "$t1" "$RECORDS" "$PEAK_WRITE" "$RC" <<'PY'
import sys
t0, t1, records, written, rc = float(sys.argv[1]), float(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
elapsed = t1 - t0
print(f"import:  {'FAILED rc='+rc if rc!='0' else 'ok'}  {elapsed:.1f}s  {records/elapsed:.1f} docs/s")
if records:
    print(f"writes:  {written/1e9:.2f} GB total  =  {written/max(records,1)/1e6:.2f} MB written per doc")
PY
fi

# --- resulting index ----------------------------------------------------------------------------
DOCS=$(python3 - "$INDEX" <<'PY'
import sqlite3, sys
try:
    c = sqlite3.connect(f"file:{sys.argv[1]}/meta/meta.sqlite?mode=ro", uri=True)
    print(c.execute("SELECT COUNT(*) FROM content").fetchone()[0])
except Exception:
    print(0)
PY
)
SIZE=$(du -sh "$INDEX" 2>/dev/null | cut -f1)
SEGMENTS=$(python3 - "$INDEX" <<'PY'
import json, sys, pathlib
base = pathlib.Path(sys.argv[1]) / "tantivy" / "base"
meta = base / "meta.json"
try:
    print(len(json.load(open(meta)).get("segments", [])))
except Exception:
    print("?")
PY
)
echo "result:  docs=$DOCS  size=$SIZE  base_segments=$SEGMENTS"

# --- publish/commit timings from the run log -----------------------------------------------------
PUB=$(grep -c "published index manifest" "$LOG" 2>/dev/null || echo 0)
echo "publishes: $PUB  (one per batch; lower is better) "
echo "(log: $LOG)"
