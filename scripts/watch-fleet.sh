#!/usr/bin/env bash
# Keep the four fleet crawlers alive.
#
# Why this exists: the crawlers have died silently twice in one day — once because a `nohup … &`
# child was killed with its parent's cgroup, once because a hung process kept a PID (and 100% CPU)
# while writing nothing for an hour, so a process count said "4 up" and the fleet produced zero
# documents. Presence is not liveness. This loop checks both, and the only durable signal it trusts
# is the shard log's mtime: a crawler that has not written a line in STALE_SECONDS is not crawling,
# whatever its PID says.
#
# It restarts (`start-crawler.sh`), never reindexes, and never touches a shard whose log is moving —
# restarting a healthy crawler loses its in-flight leases and re-does robots.txt for every host.
#
# Run under a unit (`fleet-watch.service`, Restart=always); it loops forever by design.
set -u
cd /root/projects/search || exit 1

STALE_SECONDS="${STALE_SECONDS:-900}"   # 15 min with no log line = not crawling
INTERVAL="${INTERVAL:-300}"             # check every 5 min
LOG=/var/lib/search-fleet/fleet-watch.log

log() { printf '%s %s\n' "$(date -Is)" "$*" >> "$LOG"; }

log "watch starting (stale=${STALE_SECONDS}s interval=${INTERVAL}s)"
while true; do
  now=$(date +%s)
  for i in 0 1 2 3; do
    shard_log="/var/lib/search-fleet/shard-$i.log"
    if ! pgrep -f "agent0 crawl /var/lib/search-fleet/work/shard-$i.txt" >/dev/null; then
      log "shard-$i: no crawler process; starting"
      bash scripts/start-crawler.sh "$i" >> "$LOG" 2>&1
      continue
    fi
    if [ -f "$shard_log" ]; then
      age=$(( now - $(stat -c %Y "$shard_log") ))
      if [ "$age" -gt "$STALE_SECONDS" ]; then
        log "shard-$i: log silent for ${age}s; killing and restarting"
        pkill -f "agent0 crawl /var/lib/search-fleet/work/shard-$i.txt" 2>/dev/null
        sleep 2
        bash scripts/start-crawler.sh "$i" >> "$LOG" 2>&1
      fi
    fi
  done
  sleep "$INTERVAL"
done