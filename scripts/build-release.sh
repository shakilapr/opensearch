#!/usr/bin/env bash
#
# build-release.sh — build the release binaries and stage them in dist/.
#
# Produces the three real binaries in this workspace:
#   agent0      (crates/agent0, package search-agent0)  — ingest | search | serve | refresh
#   search-mcp  (crates/mcp,    package search-mcp)     — stdio MCP server
#   bench       (crates/bench,  package search-bench)   — Stage-2 ablation harness
#
# There is NO `search-api` binary: the HTTP API is served by `agent0 serve`
# (crates/api is a library wired in behind the `real-indexer` feature, which
# crates/agent0 already enables). See docs/specs/DISTRIBUTION.md and deploy/README.md.
#
# Usage:
#   scripts/build-release.sh [--out DIR] [--no-verify]
#
#   --out DIR     stage directory (default: dist)
#   --no-verify   skip the post-copy checksum re-verification step
#
# No secrets, no network beyond the cargo registry, no sudo.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="dist"
VERIFY=1

while [[ $# -gt 0 ]]; do
    case "$1" in
        --out)
            [[ $# -ge 2 ]] || { echo "error: --out needs a value" >&2; exit 2; }
            OUT_DIR="$2"
            shift 2
            ;;
        --no-verify)
            VERIFY=0
            shift
            ;;
        -h|--help)
            sed -n '2,22p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "error: unknown argument '$1'" >&2
            exit 2
            ;;
    esac
done

cd "$REPO_ROOT"

# Binaries staged, as "package:binname".
BINS=(
    "search-agent0:agent0"
    "search-mcp:search-mcp"
    "search-bench:bench"
)

echo "==> cargo build --release (repo: $REPO_ROOT)"
for spec in "${BINS[@]}"; do
    pkg="${spec%%:*}"
    bin="${spec##*:}"
    echo "    - $pkg ($bin)"
    cargo build --release -p "$pkg" --bin "$bin"
done

BIN_DIR="$OUT_DIR/bin"
rm -rf "$BIN_DIR"
mkdir -p "$BIN_DIR"

MANIFEST="$OUT_DIR/manifest.txt"
{
    echo "# search build manifest"
    echo "generated_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "git_commit=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
    echo "rustc=$(rustc --version 2>/dev/null || echo unknown)"
    echo "cargo=$(cargo --version 2>/dev/null || echo unknown)"
    echo "# artifact  size_bytes  sha256"
} > "$MANIFEST"

for spec in "${BINS[@]}"; do
    bin="${spec##*:}"
    src="target/release/$bin"
    if [[ ! -x "$src" ]]; then
        echo "error: expected release binary not found: $src" >&2
        exit 1
    fi
    install -m 0755 "$src" "$BIN_DIR/$bin"
    size="$(stat -c %s "$BIN_DIR/$bin")"
    sum="$(sha256sum "$BIN_DIR/$bin" | awk '{print $1}')"
    printf '%s  %s  %s\n' "$bin" "$size" "$sum" >> "$MANIFEST"
done

echo
echo "==> staged artifacts in $BIN_DIR"
cat "$MANIFEST"

if [[ "$VERIFY" -eq 1 ]]; then
    echo
    echo "==> verifying staged checksums"
    while read -r name size sum; do
        # Skip the comment header and the key=value metadata lines; only
        # "<bin>  <size>  <sha256>" rows have all three fields.
        [[ "$name" == \#* || "$name" == *=* || -z "$name" || -z "$size" || -z "$sum" ]] && continue
        actual="$(sha256sum "$BIN_DIR/$name" | awk '{print $1}')"
        if [[ "$actual" != "$sum" ]]; then
            echo "error: checksum mismatch for $name ($sum != $actual)" >&2
            exit 1
        fi
        echo "    ok  $name"
    done < "$MANIFEST"
fi

echo
echo "done. manifest written to $MANIFEST"
