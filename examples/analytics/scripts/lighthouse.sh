#!/bin/sh
# Lighthouse CI against a running Kestrel (lighthouserc.cjs): signs in as
# the demo's nadia for the dashboard's cookie, works the CPU slowdown out
# for this machine (scripts/cpu-slowdown.mjs), runs the assertions, then
# prints each page's median run.
#
#   KESTREL_URL        http://127.0.0.1:3000
#   CHROME_PATH        Chrome or Chromium, when it is not where Lighthouse looks
#   LHCI_CPU_SLOWDOWN  a slowdown of your own, nothing worked out
#   LHCI_CPU_TARGET    another device than lighthouserc.cjs's (scripts/cpu-slowdown.mjs)
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"
app=${KESTREL_URL:-http://127.0.0.1:3000}
LHCI_COOKIE=$(curl -s -o /dev/null -D - -H "origin: $app" -d "name=nadia&password=${KESTREL_DEMO_PASSWORD:-kestrel-demo}" "$app/signin" |
    sed -n 's/^[Ss]et-[Cc]ookie: \([^;]*\).*/\1/p')
[ -n "$LHCI_COOKIE" ] || { echo "could not sign in to $app" >&2; exit 1; }
export LHCI_COOKIE KESTREL_URL="$app"
if [ -z "${LHCI_CPU_SLOWDOWN:-}" ]; then
    cpu=$(node scripts/cpu-slowdown.mjs)
    LHCI_CPU_SLOWDOWN=${cpu% *} LHCI_CPU_INDEX=${cpu#* }
    export LHCI_CPU_INDEX
fi
export LHCI_CPU_SLOWDOWN
rm -rf .lighthouseci
status=0
npx lhci autorun || status=$?
node scripts/lighthouse-summary.mjs
exit $status
