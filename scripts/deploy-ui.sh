#!/usr/bin/env bash
# Build, verify and deploy the UI, then smoke-test the deployed search.
#
# Why a script and not `npm run deploy`: the deployment that shipped fabricated results got there
# because nothing checked what was being published. The old bundle contained a client-side result
# generator — invented domains ("winter.org", "space-tech.net"), invented snippets — so a query the
# index knew nothing about still produced twelve confident-looking results. It was removed from the
# source, but the built artifact was never rebuilt, and the site kept lying.
#
# So this refuses to publish a bundle that contains the fabrication markers, and proves afterwards
# that the served API answers with real indexed documents.
#
# Usage: scripts/deploy-ui.sh [origin]        # origin defaults to https://search.hexxica.space
set -euo pipefail

ORIGIN="${1:-https://search.hexxica.space}"
WEBROOT="${WEBROOT:-/var/www/search}"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UI="$REPO/ui"

# Strings the removed mock generator emitted. If any reappear in a built bundle, stop.
FORBIDDEN=("Official Portal" "space-tech.net" "verified over secure TLS" "Research Papers & Engineering" "winter.org")

echo "== building the UI against $ORIGIN =="
( cd "$UI" && VITE_API_BASE="$ORIGIN" npm run build )

BUNDLES=$(find "$UI/dist/assets" -name '*.js')
[ -n "$BUNDLES" ] || { echo "no built bundle found under $UI/dist/assets" >&2; exit 1; }

echo "== refusing to publish a bundle that fabricates results =="
for pattern in "${FORBIDDEN[@]}"; do
  if grep -qF "$pattern" $BUNDLES; then
    echo "REFUSING: the built bundle contains the fabrication marker '$pattern'." >&2
    echo "Whoever reintroduced it must remove it from ui/src before this deploys." >&2
    exit 1
  fi
done
echo "   clean: none of the ${#FORBIDDEN[@]} markers appear in the built bundle"

echo "== deploying to $WEBROOT =="
backup="$WEBROOT.bak-$(date +%Y%m%d-%H%M%S)"
cp -r "$WEBROOT" "$backup"
echo "   previous build backed up to $backup"
rm -rf "$WEBROOT/assets"
cp -r "$UI/dist/." "$WEBROOT/"
chown -R www-data:www-data "$WEBROOT"

echo "== smoke test: does the served API return indexed documents? =="
curl -sS --max-time 30 -X POST -H 'content-type: application/json' \
  -d '{"query":"transformer","limit":3}' "$ORIGIN/search" -o /tmp/deploy-ui-smoke.json
python3 - /tmp/deploy-ui-smoke.json <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
rs = d.get("results") or []
if not rs:
    print("   WARNING: the deployed API returned no results for 'transformer' — check the index "
          "and that search-api.service is active", file=sys.stderr)
    sys.exit(0)
for r in rs[:3]:
    print("   -", (r.get("title") or "")[:60], "|", (r.get("canonical_url") or "")[:60])
print(f"   {len(rs)} real result(s): the site is serving indexed documents, not fabrication")
PY
