#!/bin/sh
# The shop end to end, as CI runs it: a fenec-server over a new file, the
# catalog seeded, the site built and started, then lint and the tests --
# correctness, security, SEO -- and, when asked, Lighthouse CI and the
# timings and load test.
#
#   FENEC_SERVER          the server binary (scripts/db.sh finds one otherwise)
#   SHOP_LIGHTHOUSE=1     Lighthouse CI's assertions on the budgets too
#   SHOP_LOAD=1           the page timings and the API load test too
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"
dir=$(mktemp -d)
fenec_port=${FENEC_PORT:-18290}
shop_port=${SHOP_PORT:-18300}
export FENEC_URL="http://127.0.0.1:$fenec_port"
export FENEC_PORT=$fenec_port
export FENEC_FILE="$dir/shop.fenec"
export SITE_URL="http://localhost:$shop_port"
export SHOP_URL=$SITE_URL
export SHOP_INSECURE_COOKIES=1 # plain http on localhost
export NEXT_TELEMETRY_DISABLED=1
db= site=
stop() {
    [ -n "$site" ] && kill "$site" 2>/dev/null || true
    [ -n "$db" ] && kill "$db" 2>/dev/null || true
    wait 2>/dev/null || true
    rm -rf "$dir"
}
trap stop EXIT INT TERM

wait_for() { # url, log
    i=0
    until curl -s -o /dev/null "$1"; do
        i=$((i + 1))
        if [ $i -gt 600 ]; then cat "$2"; exit 1; fi
        sleep 0.1
    done
}

sh scripts/db.sh >"$dir/db.log" 2>&1 &
db=$!
wait_for "$FENEC_URL/_health" "$dir/db.log"
npm run -s seed
npm run -s lint
npm run -s build >"$dir/build.log" 2>&1 || { cat "$dir/build.log"; exit 1; }
PORT=$shop_port npm run -s start >"$dir/site.log" 2>&1 &
site=$!
wait_for "$SHOP_URL/robots.txt" "$dir/site.log"
npm test
if [ -n "${SHOP_LIGHTHOUSE:-}" ]; then sh scripts/lighthouse.sh; fi
if [ -n "${SHOP_LOAD:-}" ]; then npm run -s timing && npm run -s load; fi
