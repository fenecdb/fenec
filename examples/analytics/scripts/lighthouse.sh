#!/bin/sh
# Lighthouse CI against a running Kestrel (lighthouserc.cjs): signs in as
# the demo's nadia for the dashboard's cookie, runs the assertions, then
# prints each page's median run.
#
#   KESTREL_URL  http://127.0.0.1:3000
#   CHROME_PATH  Chrome or Chromium, when it is not where Lighthouse looks
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"
app=${KESTREL_URL:-http://127.0.0.1:3000}
LHCI_COOKIE=$(curl -s -o /dev/null -D - -H "origin: $app" -d "name=nadia&password=${KESTREL_DEMO_PASSWORD:-kestrel-demo}" "$app/signin" |
    sed -n 's/^[Ss]et-[Cc]ookie: \([^;]*\).*/\1/p')
[ -n "$LHCI_COOKIE" ] || { echo "could not sign in to $app" >&2; exit 1; }
export LHCI_COOKIE KESTREL_URL="$app"
rm -rf .lighthouseci
status=0
npx lhci autorun || status=$?
node scripts/lighthouse-summary.mjs
exit $status
