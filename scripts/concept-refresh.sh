#!/usr/bin/env bash
# Keep the concept matrix current as the corpus grows.
#
# The matrix is built by a batch pass, and a batch pass only describes the corpus at the moment it
# ran. Measured 2026-11-02: the crawlers had added ~246,000 documents since the last full pass (15%
# of the corpus), and every one of them was invisible to the concept lane — the matrix "worked",
# it was just describing an older corpus. Nothing surfaced that; the lane simply returned fewer
# candidates.
#
# This loop closes the gap two ways:
#
#   full   (`--full`)   one pass over the whole root, for the first run or after a lexicon change.
#   steady (default)    `concepts build --since <checkpoint>` every INTERVAL seconds, then
#                       `concepts relate` so the co-occurrence graph includes the new documents.
#
# The checkpoint is a per-root file holding the last *fetched_at* covered, minus a small overlap so
# a document committed while the pass was reading is not skipped. Tagging is an upsert keyed on
# (resource, concept, kind), so re-tagging an overlapped document is idempotent.
#
# Run under a unit (`concept-refresh.service`, Restart=always). It is deliberately a loop and not a
# systemd timer: the pass can outlive the interval, and two passes on one root would fight for the
# SQLite writer.
set -u
cd /root/projects/search || exit 1

BIN=./target/release/agent0
ROOTS=(/var/lib/search /var/lib/search-fleet/shard-0 /var/lib/search-fleet/shard-1 /var/lib/search-fleet/shard-2 /var/lib/search-fleet/shard-3)
INTERVAL="${INTERVAL:-900}"     # 15 min
OVERLAP="${OVERLAP:-120}"       # re-scan the last 2 min of documents
STATE=/var/lib/search-fleet/concept-checkpoints
LOG=/var/lib/search-fleet/concept-refresh.log

mkdir -p "$STATE"
log() { printf '%s %s\n' "$(date -Is)" "$*" | tee -a "$LOG"; }

refresh_root() {
  local root="$1" mode="$2"
  local name ckpt_file ckpt
  name=$(basename "$root")
  ckpt_file="$STATE/$name"
  if [ "$mode" = "full" ]; then
    log "$name: full pass"
    if ! SEARCH_DATA_DIR="$root" "$BIN" concepts build --dir "$root" >>"$LOG" 2>&1; then
      log "$name: full pass FAILED"
      return 1
    fi
    date +%s > "$ckpt_file"
  else
    ckpt=$(cat "$ckpt_file" 2>/dev/null || echo 0)
    if [ "$ckpt" -le 0 ]; then
      log "$name: no checkpoint; skipping incremental (run with --full once)"
      return 0
    fi
    SEARCH_DATA_DIR="$root" "$BIN" concepts build --dir "$root" --since "$((ckpt - OVERLAP))" >>"$LOG" 2>&1 \
      || { log "$name: incremental build FAILED"; return 1; }
    date +%s > "$ckpt_file"
  fi
  SEARCH_DATA_DIR="$root" "$BIN" concepts relate --dir "$root" >>"$LOG" 2>&1 \
    || log "$name: relate FAILED (graph left at the previous pass)"
  log "$name: ok"
}

MODE="${1:-steady}"
if [ "$MODE" = "full" ]; then
  # Full catch-up: roots in parallel. Each has its own database, so there is no shared writer; the
  # machine has 16 cores and tagging is the only heavy work.
  log "full catch-up across ${#ROOTS[@]} roots"
  pids=()
  for root in "${ROOTS[@]}"; do
    refresh_root "$root" full &
    pids+=($!)
  done
  failed=0
  for p in "${pids[@]}"; do wait "$p" || failed=1; done
  [ "$failed" -eq 0 ] && log "full catch-up complete" || log "full catch-up had failures"
fi

while true; do
  log "steady pass"
  for root in "${ROOTS[@]}"; do
    refresh_root "$root" steady
  done
  sleep "$INTERVAL"
done