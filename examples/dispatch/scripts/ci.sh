#!/bin/sh
# Plover end to end, as CI runs it: lint; a cluster of its own (two tenant
# nodes and fenec-shard over a new directory) set up with two days of
# history; the site built and started on it; then the tests -- the claims,
# leases, quotas and security files each start a cluster of their own, the
# pages file runs against this one. DISPATCH_BENCH=1 adds the measurements.
#
# Every port is one the system picks, so two runs -- or this beside any
# other suite or server -- never meet on one.
#
#   FENEC_SERVER, FENEC_SHARD   the binaries (this repository's target/release, then target/debug)
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"
dir=$(mktemp -d)
export PLOVER_DATA="$dir"
export PLOVER_ROUTER_PORT=0
export PLOVER_HISTORY_DAYS=2
export NEXT_TELEMETRY_DISABLED=1
cluster= site= hold=
stop() {
    [ -n "$site" ] && kill "$site" 2>/dev/null || true
    [ -n "$hold" ] && kill "$hold" 2>/dev/null || true
    [ -n "$cluster" ] && kill "$cluster" 2>/dev/null || true
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

npm run -s lint

# The site's port. The cluster's CORS names the site's origin before the
# site starts, and `next start` binds the port it is given rather than
# saying which it took, so the site cannot listen on :0 as the cluster's
# processes do. The port is one the system picks, held by a listener
# standing in for the site -- the system hands no one a port a listener
# holds -- and let go of the moment before the site binds it; a site that
# still loses it exits, and is waited for by its own line naming the
# port, never by a page someone else's server answers.
node -e '
const s = require("node:net").createServer().listen(0, () => require("node:fs").writeFileSync(process.argv[1], String(s.address().port)));
process.on("SIGTERM", () => s.close(() => process.exit(0)));
' "$dir/site.port" &
hold=$!
until_line . "$dir/site.port" "$hold"
site_port=$(cat "$dir/site.port")
export PLOVER_ORIGIN="http://localhost:$site_port"
export PLOVER_SITE=$PLOVER_ORIGIN

npm run -s cluster >"$dir/cluster.log" 2>&1 &
cluster=$!
# Up once the router has its nodes, which it prints with where it listens.
until_line '^router ' "$dir/cluster.log" "$cluster"
PLOVER_ROUTER=$(sed -n 's/^router \([^,]*\),.*/\1/p' "$dir/cluster.log")
export PLOVER_ROUTER
npm run -s setup
npm run -s build >"$dir/build.log" 2>&1 || { cat "$dir/build.log"; exit 1; }
kill "$hold" && wait "$hold" || true
hold=
PORT=$site_port npm run -s start >"$dir/site.log" 2>&1 &
site=$!
until_line "Local: +http://localhost:$site_port( |\$)" "$dir/site.log" "$site"
wait_for "$PLOVER_SITE/signin" "$dir/site.log"
npm test
if [ -n "${DISPATCH_BENCH:-}" ]; then npm run -s bench; fi
