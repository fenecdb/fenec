#!/bin/sh
# Lighthouse CI against a running shop (lighthouserc.cjs): the product page
# is the first tent's, looked up here, Chrome's files are read once before
# its first launch (scripts/warm-chrome.mjs), the CPU slowdown is worked out
# for this machine (scripts/cpu-slowdown.mjs), and a summary of each page's
# median run is printed after the assertions.
#
#   SHOP_URL           http://localhost:3000
#   CHROME_PATH        Chrome or Chromium, when it is not where Lighthouse looks
#   LHCI_CPU_SLOWDOWN  a slowdown of your own, nothing worked out
#   LHCI_CPU_TARGET    another device than lighthouserc.cjs's (scripts/cpu-slowdown.mjs)
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"
shop=${SHOP_URL:-http://localhost:3000}
slug=$(node -e "fetch('$shop/api/c/tents').then(r=>r.json()).then(b=>console.log(b.cards[0].slug))")
export LHCI_PRODUCT="/p/$slug"
node scripts/warm-chrome.mjs
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
