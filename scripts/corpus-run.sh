#!/usr/bin/env bash
# Stage-1 corpus run: build release `agent0` and crawl the seed list into
# `raw-dataset/` (the Tier-3 raw blob cache, work-plan.md §Stage 0).
#
# Idempotent and resumable: the crawl log and the frontier live in `--out`, and
# every URL with a terminal outcome is skipped on a re-run.
#
#   SEARCH_DATA_DIR  metadata + Tantivy index + blobs (server-only, gitignored)
#   --out            raw-dataset: raw blobs, crawl-log.jsonl, frontier.json
set -euo pipefail

cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

SEEDS="${SEEDS:-crawler/seeds/llm-tech.txt}"
OUT="${OUT:-raw-dataset}"
DATA_DIR="${SEARCH_DATA_DIR:-/var/lib/search}"
MAX_DOCS="${MAX_DOCS:-10000}"
MAX_URLS="${MAX_URLS:-200000}"
CONCURRENCY="${CONCURRENCY:-16}"
ORIGIN_INTERVAL_MS="${ORIGIN_INTERVAL_MS:-750}"
# Per-origin attempt cap: a small value spreads the document budget across many
# sources (the Stage-1 goal), the derived default is depth-first.
ORIGIN_CAP="${ORIGIN_CAP:-}"

echo "== building release agent0 (bge off: lexical corpus build) =="
cargo build --release -p search-agent0

BIN=target/release/agent0
mkdir -p "$OUT/blobs"

echo "== crawling seeds=$SEEDS -> out=$OUT (max-docs=$MAX_DOCS max-urls=$MAX_URLS origin-cap=${ORIGIN_CAP:-derived})"
SEARCH_DATA_DIR="$DATA_DIR" \
SEARCH_CRAWL_ORIGIN_INTERVAL_MS="$ORIGIN_INTERVAL_MS" \
  "$BIN" crawl "$SEEDS" \
    --max-docs "$MAX_DOCS" \
    --max-urls "$MAX_URLS" \
    --concurrency "$CONCURRENCY" \
    ${ORIGIN_CAP:+--origin-cap "$ORIGIN_CAP"} \
    --out "$OUT"

echo "== raw-dataset =="
echo "blobs: $(find "$OUT/blobs" -type f | wc -l)"
echo "log lines: $(wc -l < "$OUT/crawl-log.jsonl")"

echo "== corpus summary =="
python3 - "$OUT/crawl-log.jsonl" <<'PY'
import collections, json, sys, urllib.parse
counts, hosts = collections.Counter(), collections.Counter()
for line in open(sys.argv[1], errors="replace"):
    try:
        r = json.loads(line)
    except ValueError:
        continue
    counts[r.get("outcome")] += 1
    if r.get("outcome") == "ingested":
        hosts[urllib.parse.urlparse(r.get("url", "")).netloc] += 1
print("attempts:", sum(counts.values()), dict(counts))
print("sources:", len(hosts))
print("top sources:", hosts.most_common(10))
PY
