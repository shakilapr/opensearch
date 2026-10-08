#!/usr/bin/env bash
#
# backup.sh — take a consistent backup of the search data root (/var/lib/search).
#
# SAFETY: a raw `cp` of a *live* SQLite WAL database or a Tantivy MmapDirectory
# copied mid-write is CORRUPT. WAL only recovers if the -wal/-shm siblings and the
# main db are copied as one atomic unit; an mmap'ed Tantivy segment can be read in
# a torn state. This script therefore STOPS the writer service(s) first, copies the
# tree while nothing is writing, then restarts them. If you pass --no-stop you get
# a loud warning and a best-effort copy — only do that if you know the writer is idle.
#
# Usage:
#   scripts/backup.sh [--data-dir DIR] [--out DIR] [--unit NAME]... [--no-stop] [--no-tar]
#
#   --data-dir DIR   runtime state root         (default: /var/lib/search)
#   --out DIR        backup destination root    (default: /var/backups/search)
#   --unit NAME      systemd unit to stop first (repeatable; default: the search-* units)
#   --no-stop        do NOT stop/start services (unsafe for a live writer; warns)
#   --no-tar         keep the copied directory, skip the .tar.gz + sha256
#
# Restore is the inverse: stop the service, replace the data root with the backup's
# data/, restart. See docs/OPERATIONS.md ("Restore").
#
# No secrets, no network.
set -euo pipefail

DATA_DIR="/var/lib/search"
OUT_DIR="/var/backups/search"
NO_STOP=0
NO_TAR=0
UNITS=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --data-dir) [[ $# -ge 2 ]] || { echo "error: --data-dir needs a value" >&2; exit 2; }; DATA_DIR="$2"; shift 2 ;;
        --out)      [[ $# -ge 2 ]] || { echo "error: --out needs a value" >&2; exit 2; };      OUT_DIR="$2";  shift 2 ;;
        --unit)     [[ $# -ge 2 ]] || { echo "error: --unit needs a value" >&2; exit 2; };     UNITS+=("$2"); shift 2 ;;
        --no-stop)  NO_STOP=1; shift ;;
        --no-tar)   NO_TAR=1; shift ;;
        -h|--help)  sed -n '2,27p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *)          echo "error: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

if [[ ! -d "$DATA_DIR" ]]; then
    echo "error: data dir not found: $DATA_DIR" >&2
    exit 1
fi

if [[ ${#UNITS[@]} -eq 0 ]]; then
    UNITS=(search-agent.service search-api.service)
fi

TS="$(date -u +%Y%m%dT%H%M%SZ)"
DEST="$OUT_DIR/search-$TS"
STOPPED=()

stop_services() {
    command -v systemctl >/dev/null 2>&1 || return 0
    for u in "${UNITS[@]}"; do
        if systemctl is-active --quiet "$u" 2>/dev/null; then
            echo "    stopping $u"
            systemctl stop "$u"
            STOPPED+=("$u")
        fi
    done
}

start_services() {
    local u
    for u in "${STOPPED[@]:-}"; do
        [[ -z "$u" ]] && continue
        echo "    starting $u"
        systemctl start "$u" || echo "warning: failed to restart $u" >&2
    done
}

if [[ "$NO_STOP" -eq 1 ]]; then
    echo "WARNING: --no-stop set. A raw copy of a live SQLite/Tantivy data root is UNSAFE:" >&2
    echo "         the backup may be torn or corrupt if the writer is active." >&2
else
    echo "==> stopping writer service(s) for a consistent copy"
    stop_services
    # Always try to bring the services back, even on error.
    trap start_services EXIT
fi

echo "==> copying $DATA_DIR -> $DEST/data"
mkdir -p "$DEST"
if command -v rsync >/dev/null 2>&1; then
    rsync -a --delete "$DATA_DIR/" "$DEST/data/"
else
    mkdir -p "$DEST/data"
    cp -a "$DATA_DIR/." "$DEST/data/"
fi

# Record what we captured (best-effort; never fails the run).
{
    echo "data_dir=$DATA_DIR"
    echo "backup_utc=$TS"
    echo "stopped=${STOPPED[*]:-none}"
    echo "files=$(find "$DEST/data" -type f | wc -l)"
    echo "bytes=$(du -sb "$DEST/data" | awk '{print $1}')"
} > "$DEST/BACKUP_INFO.txt"

if [[ "$NO_TAR" -eq 0 ]]; then
    echo "==> archiving"
    tar -C "$DEST" -czf "$DEST.tar.gz" data BACKUP_INFO.txt
    sha256sum "$DEST.tar.gz" > "$DEST.tar.gz.sha256"
    echo "    $DEST.tar.gz"
    echo "    $(cat "$DEST.tar.gz.sha256")"
fi

echo
echo "done. backup at $DEST"
cat "$DEST/BACKUP_INFO.txt"
