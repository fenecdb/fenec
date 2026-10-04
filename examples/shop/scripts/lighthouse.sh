#!/bin/sh
# Lighthouse CI against a running shop (lighthouserc.cjs): the product page
# is the first tent's, looked up here, and a summary of each page's median
# run is printed after the assertions.
#
#   SHOP_URL     http://localhost:3000
#   CHROME_PATH  Chrome or Chromium, when it is not where Lighthouse looks
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"
shop=${SHOP_URL:-http://localhost:3000}
slug=$(node -e "fetch('$shop/api/c/tents').then(r=>r.json()).then(b=>console.log(b.cards[0].slug))")
export LHCI_PRODUCT="/p/$slug"
rm -rf .lighthouseci
status=0
npx lhci autorun || status=$?
node scripts/lighthouse-summary.mjs
exit $status
