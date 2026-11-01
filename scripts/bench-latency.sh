#!/usr/bin/env bash
# Measure /search latency over a fixed query sample and report the distribution.
#
# Why a harness and not a few curls: the host is crawling, so any single timing varies by 2x
# between runs independent of the code. Comparing strategies needs the same sample, the same number
# of repetitions and a distribution (p50/p95/max), not one lucky number.
#
# Usage: scripts/bench-latency.sh [reps] [label]
#   reps   repetitions per query (default 3)
#   label  tag for the output line (default "run")
set -uo pipefail

REPS="${1:-3}"
LABEL="${2:-run}"
URL="${SEARCH_URL:-http://127.0.0.1:8090/search}"

# A fixed sample spanning the query shapes that matter: one word, a common phrase, a long
# conceptual question, an exact identifier, and non-ASCII.
QUERIES=(
  "testing"
  "machine learning"
  "chemical reactions that affect romance"
  "what molecule makes people bond with each other"
  "tokio runtime"
  "Кубернетес логи"
)

times=()
for q in "${QUERIES[@]}"; do
  for _ in $(seq 1 "$REPS"); do
    # -w gives total wall time for the request including connection setup.
    t=$(curl -s -o /dev/null -w "%{time_total}" -X POST "$URL" \
        -H 'content-type: application/json' \
        -d "{\"query\":\"$q\",\"limit\":10,\"engine\":true}")
    times+=("$t")
  done
done

# Percentiles via python: bash arithmetic cannot sort floats.
printf '%s\n' "${times[@]}" | python3 -c "
import sys
vals = sorted(float(x) for x in sys.stdin if x.strip())
n = len(vals)
def pct(p):
    if not vals: return 0.0
    i = min(n - 1, max(0, int(round(p / 100.0 * (n - 1)))))
    return vals[i]
print(f'  {\"$LABEL\":22} n={n:3d}  p50={pct(50):6.3f}s  p95={pct(95):6.3f}s  max={vals[-1]:6.3f}s  mean={sum(vals)/n:6.3f}s')
"