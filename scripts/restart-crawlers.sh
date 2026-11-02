#!/usr/bin/env bash
# Restart the four fleet crawlers on the current binary.
#
# Uses the fixed binary (append-delta frontier, no duplicate blob output, busy-retry
# metadata, disk guard) and a higher in-flight bound: the network probe measured p50 0.3 s
# / mean 0.8 s per seed, so 32 in flight left the box at 8% CPU.
#
# Each crawler is its own transient systemd unit (see `start-crawler.sh`): a `nohup ... &` child
# dies with the cgroup of whatever started it, which is how four "restarted" crawlers vanished after
# a rebuild on 2026-11-02.
set -u
cd /root/projects/search || exit 1

for i in 0 1 2 3; do
  systemctl stop "crawler-shard-$i" 2>/dev/null
  for _ in 1 2 3 4 5; do
    pgrep -f "agent0 crawl /var/lib/search-fleet/work/shard-$i.txt" >/dev/null || break
    sleep 1
  done
  # A crawler started outside systemd (an older script) still needs the signal.
  pkill -f "agent0 crawl /var/lib/search-fleet/work/shard-$i.txt" 2>/dev/null
done
sleep 2

# The duplicate raw spill under the crawl output is no longer written by this binary, so the
# existing copies are dead weight; removing them frees space.
for i in 0 1 2 3; do
  rm -rf "raw-dataset/shard-$i/blobs"
done

for i in 0 1 2 3; do
  bash scripts/start-crawler.sh "$i"
done
sleep 2
echo "running: $(pgrep -fc 'agent0 crawl /var/lib/search-fleet/work/shard-' 2>/dev/null || echo 0)"