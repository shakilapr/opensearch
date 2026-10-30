#!/usr/bin/env bash
# Measure the graded query set under every lane configuration, so a fusion weight is a decision
# backed by numbers rather than a guess.
#
# PLAN-v2 §5's gates are all "strictly better on 600 graded queries". That sentence requires a
# harness that produces the same number twice, which is what this script establishes: same set,
# same mode (Fast, no live fetches), same limit, one process per configuration so a lane cannot
# leak into the next run.
#
# Usage: scripts/eval-matrix.sh [data-dir]
set -uo pipefail

cd /root/projects/search || exit 1
BIN=./target/release/agent0
DIR="${1:-/var/lib/search}"
SET=benchmarks/queries.corpus.jsonl
OUT=/var/lib/search-fleet/eval
mkdir -p "$OUT"

run() {
    local name="$1" lanes="$2" fusion="$3"
    echo "=== $name (lanes=$lanes ${fusion:-default weights}) ==="
    # `env -u` every fusion variable first, so one configuration cannot inherit another's weight.
    env -u SEARCH_FUSION_CONCEPT -u SEARCH_FUSION_DENSE -u SEARCH_FUSION_GRAPH \
        -u SEARCH_FUSION_BM25 -u SEARCH_FUSION_SOURCE_FIRST \
        SEARCH_DATA_DIR="$DIR" SEARCH_MODEL_CACHE=/models/fastembed \
        $fusion "$BIN" eval run --set "$SET" --lanes "$lanes" --mode fast \
        --out "$OUT/$name.json" 2>&1 | grep -E "^  (n|ndcg_at_10|mrr|recall_at_10|p50_ms|p95_ms)|^lanes"
}

# Baseline: the two lexical lanes only, which is what the engine has always done.
run bm25_only none ""

# Each semantic lane alone, at a weight high enough to move ranking if it can.
run dense_only dense "SEARCH_FUSION_DENSE=0.8"
run concept_only concept "SEARCH_FUSION_CONCEPT=0.8"
run graph_only graph "SEARCH_FUSION_GRAPH=0.8"

# All lanes, to see whether they compose.
run all_lanes all "SEARCH_FUSION_DENSE=0.5 SEARCH_FUSION_CONCEPT=0.8 SEARCH_FUSION_GRAPH=0.3"

echo
echo "=== summary ==="
python3 - "$OUT" <<'PY'
import json, sys, pathlib
d = pathlib.Path(sys.argv[1])
rows = []
for f in ["bm25_only", "dense_only", "concept_only", "graph_only", "all_lanes"]:
    p = d / f"{f}.json"
    if not p.is_file():
        rows.append((f, None)); continue
    rows.append((f, json.loads(p.read_text())))
base = rows[0][1]
print(f"  {'config':14} {'nDCG@10':>9} {'Δ':>8} {'recall@10':>10} {'MRR':>7} {'p50':>7} {'p95':>7}")
for name, m in rows:
    if m is None:
        print(f"  {name:14} {'(no result)':>9}")
        continue
    delta = m["ndcg_at_10"] - base["ndcg_at_10"] if base else 0.0
    print(f"  {name:14} {m['ndcg_at_10']:9.4f} {delta:+8.4f} {m['recall_at_10']:10.4f} "
          f"{m['mrr']:7.4f} {m['p50_ms']:7.0f} {m['p95_ms']:7.0f}")
PY
