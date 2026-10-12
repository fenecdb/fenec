#!/bin/sh
# The shop end to end, as CI runs it: a fenec-server over a new file, the
# catalog seeded, the site built and started, then lint and the tests --
# correctness, security, SEO -- and, when asked, Lighthouse CI and the
# timings and load test.
#
# Every port is one the system picks, so two runs -- or this beside any
# other suite or server -- never meet on one.
#
#   FENEC_SERVER          the server binary (scripts/db.sh finds one otherwise)
#   SHOP_LIGHTHOUSE=1     Lighthouse CI's assertions on the budgets too
#   SHOP_LOAD=1           the page timings and the API load test too
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"
dir=$(mktemp -d)
export FENEC_PORT=0
export FENEC_FILE="$dir/shop.fenec"
export SHOP_INSECURE_COOKIES=1 # plain http on localhost
export NEXT_TELEMETRY_DISABLED=1
db= site= hold=
stop() {
    [ -n "$site" ] && kill "$site" 2>/dev/null || true
    [ -n "$hold" ] && kill "$hold" 2>/dev/null || true
    [ -n "$db" ] && kill "$db" 2>/dev/null || true
    wait 2>/dev/null || true
    rm -rf "$dir"
}
trap stop EXIT INT TERM

until_line() { # pattern (ERE), log, pid: until the log holds a line matching it, or the process is gone
    i=0
    until grep -Eq "$1" "$2" 2>/dev/null; do
        i=$((i + 1))
        if [ $i -gt 600 ] || ! kill -0 "$3" 2>/dev/null; then cat "$2"; exit 1; fi
        sleep 0.1
    done
}

wait_for() { # url, log
    i=0
    until curl -s -o /dev/null "$1"; do
        i=$((i + 1))
        if [ $i -gt 600 ]; then cat "$2"; exit 1; fi
        sleep 0.1
    done
}

# The site's port. Its address is in the pages the build renders (the
# canonical links, the sitemaps) and in the server's CORS, both before the
# site starts, and `next start` binds the port it is given rather than
# saying which it took, so the site cannot listen on :0 as the server
# does. The port is one the system picks, held by a listener standing in
# for the site -- the system hands no one a port a listener holds -- and
# let go of the moment before the site binds it; a site that still loses
# it exits, and is waited for by its own line naming the port, never by a
# page someone else's server answers.
node -e '
const s = require("node:net").createServer().listen(0, () => require("node:fs").writeFileSync(process.argv[1], String(s.address().port)));
process.on("SIGTERM", () => s.close(() => process.exit(0)));
' "$dir/site.port" &
hold=$!
until_line . "$dir/site.port" "$hold"
shop_port=$(cat "$dir/site.port")
export SITE_URL="http://localhost:$shop_port"
export SHOP_URL=$SITE_URL

# The server on :0, its url read off its `listening on: http://<address>` line.
sh scripts/db.sh >"$dir/db.log" 2>&1 &
db=$!
until_line 'listening on: http://[^ ]+ ' "$dir/db.log" "$db"
FENEC_URL=$(sed -n 's/.*listening on: \(http:[^ ]*\) .*/\1/p' "$dir/db.log" | head -1)
export FENEC_URL
wait_for "$FENEC_URL/_health" "$dir/db.log"
npm run -s seed
npm run -s lint
npm run -s build >"$dir/build.log" 2>&1 || { cat "$dir/build.log"; exit 1; }
kill "$hold" && wait "$hold" || true
hold=
PORT=$shop_port npm run -s start >"$dir/site.log" 2>&1 &
site=$!
until_line "Local: +http://localhost:$shop_port( |\$)" "$dir/site.log" "$site"
wait_for "$SHOP_URL/robots.txt" "$dir/site.log"
npm test
if [ -n "${SHOP_LIGHTHOUSE:-}" ]; then sh scripts/lighthouse.sh; fi
if [ -n "${SHOP_LOAD:-}" ]; then npm run -s timing && npm run -s load; fi
