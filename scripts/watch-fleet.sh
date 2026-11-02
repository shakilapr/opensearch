#!/usr/bin/env bash
# Keep the four fleet crawlers alive.
#
# Why this exists: the crawlers have died silently twice in one day — once because a `nohup … &`
# child was killed with its parent's cgroup, once because a hung process kept a PID (and 100% CPU)
# while writing nothing for an hour, so a process count said "4 up" and the fleet produced zero
# documents.
#
# Two independent signals, because either one alone lies:
#
#   * the shard log's mtime — a crawler that writes nothing at all (kill, hang, lock zombie);
#   * the last *ingested* attempt in `crawl-log.jsonl` — a crawler that is alive and chatty but
#     produces no documents, which is what rate-limit starvation looks like. Measured 2026-11-02:
#     one shard wrote 429 warnings for over an hour with zero ingests and every presence check said
#     it was healthy.
#
# It restarts (`start-crawler.sh`), never reindexes, and never touches a shard that is ingesting —
# restarting a healthy crawler loses its in-flight leases and re-reads robots.txt for every host.
#
# Run under a unit (`fleet-watch.service`, Restart=always); it loops forever by design.
set -u
cd /root/projects/search || exit 1

STALE_SECONDS="${STALE_SECONDS:-900}"            # 15 min with no log line at all
NO_INGEST_SECONDS="${NO_INGEST_SECONDS:-1800}"   # 30 min with nothing ingested = starving
INTERVAL="${INTERVAL:-300}"                      # check every 5 min
LOG=/var/lib/search-fleet/fleet-watch.log

log() { printf '%s %s\n' "$(date -Is)" "$*" >> "$LOG"; }

# Epoch of the most recent ingested attempt in a shard's crawl log, or 0 when unknown.
# Reads the tail of the file (bounded), because crawl-log.jsonl grows without rotation.
last_ingest_epoch() {
  python3 -c '
import json, os, sys
p = sys.argv[1]
if not os.path.exists(p):
    print(0); raise SystemExit
try:
    with open(p, "rb") as f:
        f.seek(0, os.SEEK_END)
        f.seek(max(0, f.tell() - 2_000_000))
        lines = f.read().decode("utf-8", "replace").splitlines()
except OSError:
    print(0); raise SystemExit
for line in reversed(lines):
    try:
        d = json.loads(line)
    except Exception:
        continue
    if d.get("outcome") == "ingested":
        print(int(d.get("fetched_at") or 0)); break
else:
    # The file exists but holds no ingested attempt in the window: a starved shard, not an unknown
    # one. `-1` tells the caller to treat it as stale; `0` is reserved for "cannot tell".
    print(-1)
' "$1"
}

restart() {
  local i="$1" why="$2"
  log "shard-$i: $why; killing and restarting"
  pkill -f "agent0 crawl /var/lib/search-fleet/work/shard-$i.txt" 2>/dev/null
  sleep 2
  bash scripts/start-crawler.sh "$i" >> "$LOG" 2>&1
}

log "watch starting (stale=${STALE_SECONDS}s no_ingest=${NO_INGEST_SECONDS}s interval=${INTERVAL}s)"
while true; do
  now=$(date +%s)
  for i in 0 1 2 3; do
    # Recycle expired leases every cycle. A refill leases rows as FETCHING; a URL the drain refuses
    # (low value, already logged) never reaches the completion path, so its row would sit leased
    # forever. Measured 2026-11-02: 19,963 such rows on one shard within minutes of the lease fix.
    ./target/release/agent0 frontier recover --dir "/var/lib/search-fleet/shard-$i" >> "$LOG" 2>&1 || true
    if ! pgrep -f "agent0 crawl /var/lib/search-fleet/work/shard-$i.txt" >/dev/null; then
      log "shard-$i: no crawler process; starting"
      bash scripts/start-crawler.sh "$i" >> "$LOG" 2>&1
      continue
    fi

    shard_log="/var/lib/search-fleet/shard-$i.log"
    if [ -f "$shard_log" ]; then
      age=$(( now - $(stat -c %Y "$shard_log") ))
      if [ "$age" -gt "$STALE_SECONDS" ]; then
        restart "$i" "log silent for ${age}s"
        continue
      fi
    fi

    last_ing=$(last_ingest_epoch "raw-dataset/shard-$i/crawl-log.jsonl")
    case "${last_ing:-0}" in
      0) ;;  # no log at all: nothing to judge yet
      -1) restart "$i" "no ingested attempt in the recent log" ;;
      *)
        idle=$(( now - last_ing ))
        if [ "$idle" -gt "$NO_INGEST_SECONDS" ]; then
          restart "$i" "nothing ingested for ${idle}s"
        fi
        ;;
    esac
  done
  sleep "$INTERVAL"
done