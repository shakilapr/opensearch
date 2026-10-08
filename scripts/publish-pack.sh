#!/usr/bin/env bash
#
# publish-pack.sh — stage a validated dataset pack for distribution.
#
# A pack is produced by the Stage-6 dataset builder (crate `search-dataset`):
# a directory of JSONL shards plus `manifest.json` (see crates/dataset/src/pack.rs
# and docs/DISTRIBUTION.md). This script takes an already-built pack directory,
# re-checks it against its manifest, copies it to dist/packs/<name>/<version>/,
# and prints the manifest + checksums.
#
# Usage:
#   scripts/publish-pack.sh <pack-dir> [--out DIR]
#
#   <pack-dir>    directory containing manifest.json and *.jsonl shards
#   --out DIR     stage root (default: dist)
#
# What it verifies (independently re-computed here, not trusted from the pack):
#   - manifest.json exists and parses; name/version are non-empty
#   - every shard listed in the manifest exists
#   - each shard's FNV-1a (64-bit) checksum matches the manifest `fnv1a64:...` value
#   - each shard's JSONL record count matches the manifest `records`
#   - per-shard counts sum to the manifest total
#
# It also emits a distribution-level SHA-256 of every staged file, for transfer
# integrity (the manifest's own checksums are FNV-1a, an integrity — not a
# cryptographic — check, per crates/dataset/src/checksum.rs).
#
# No secrets, no network.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="dist"

if [[ $# -lt 1 ]]; then
    echo "usage: scripts/publish-pack.sh <pack-dir> [--out DIR]" >&2
    exit 2
fi

SRC="$1"; shift
while [[ $# -gt 0 ]]; do
    case "$1" in
        --out)
            [[ $# -ge 2 ]] || { echo "error: --out needs a value" >&2; exit 2; }
            OUT_DIR="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,29p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *)
            echo "error: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

cd "$REPO_ROOT"

if [[ ! -d "$SRC" ]]; then
    echo "error: pack dir not found: $SRC" >&2
    exit 1
fi
if [[ ! -f "$SRC/manifest.json" ]]; then
    echo "error: $SRC/manifest.json not found (not a pack directory?)" >&2
    exit 1
fi

command -v python3 >/dev/null 2>&1 || { echo "error: python3 is required" >&2; exit 1; }

# --- verify the pack against its manifest (FNV-1a, structural) ----------------
read -r NAME VERSION < <(python3 - "$SRC" <<'PY'
import json, sys
p = sys.argv[1]
with open(p + "/manifest.json") as f:
    m = json.load(f)
name = str(m.get("name", "")).strip()
version = str(m.get("version", "")).strip()
if not name or not version:
    sys.exit("manifest.json: name/version must be non-empty")
print(name, version)
PY
)

echo "==> verifying pack '$NAME' version '$VERSION'"

python3 - "$SRC" <<'PY'
import json, os, sys

pack = sys.argv[1]
FNV_OFFSET = 0xcbf29ce484222325
FNV_PRIME = 0x100000001b3
MASK = 0xFFFFFFFFFFFFFFFF

def fnv1a64(data: bytes) -> int:
    h = FNV_OFFSET
    for b in data:
        h ^= b
        h = (h * FNV_PRIME) & MASK
    return h

with open(os.path.join(pack, "manifest.json")) as f:
    m = json.load(f)

shards = m.get("shards", [])
total = 0
for s in shards:
    path = s["path"]
    full = os.path.join(pack, path)
    if not os.path.isfile(full):
        sys.exit(f"missing shard: {path}")
    with open(full, "rb") as fh:
        data = fh.read()
    actual = "fnv1a64:%016x" % fnv1a64(data)
    if actual != s["checksum"]:
        sys.exit(f"checksum mismatch for {path}: manifest {s['checksum']} != on-disk {actual}")
    count = sum(
        1 for line in data.split(b"\n")
        if line.strip() and json.loads(line) is not None
    )
    if count != s["records"]:
        sys.exit(f"record count mismatch for {path}: manifest {s['records']} != on-disk {count}")
    total += count

if total != m.get("records"):
    sys.exit(f"total record count mismatch: manifest {m['records']} != shards {total}")
print(f"    ok  {len(shards)} shard(s), {total} record(s), schema={m.get('schema')}")
PY

# --- stage --------------------------------------------------------------------
DEST="$OUT_DIR/packs/$NAME/$VERSION"
rm -rf "$DEST"
mkdir -p "$DEST"
cp -a "$SRC/." "$DEST/"

# Distribution-level SHA-256 for transfer integrity.
( cd "$DEST" && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum ) > "$DEST/SHA256SUMS"

echo
echo "==> staged to $DEST"
echo "--- manifest.json ---"
cat "$DEST/manifest.json"
echo
echo "--- SHA256SUMS ---"
cat "$DEST/SHA256SUMS"
echo
echo "done."
