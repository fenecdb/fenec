#!/bin/sh
# Plover end to end, as CI runs it: lint; a cluster of its own (two tenant
# nodes and fenec-shard over a new directory) set up with two days of
# history; the site built and started on it; then the tests -- the claims,
# leases, quotas and security files each start a cluster of their own, the
# pages file runs against this one. DISPATCH_BENCH=1 adds the measurements.
#
#   FENEC_SERVER, FENEC_SHARD   the binaries (this repository's target/release, then target/debug)
#   DISPATCH_PORT               the router's port (the nodes take the next two); the site's is past them
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"
dir=$(mktemp -d)
router_port=${DISPATCH_PORT:-18460}
site_port=$((router_port + 3))
export PLOVER_DATA="$dir"
export PLOVER_ROUTER_PORT=$router_port
export PLOVER_ROUTER="http://127.0.0.1:$router_port"
export PLOVER_ORIGIN="http://localhost:$site_port"
export PLOVER_SITE=$PLOVER_ORIGIN
export PLOVER_HISTORY_DAYS=2
export NEXT_TELEMETRY_DISABLED=1
cluster= site=
stop() {
    [ -n "$site" ] && kill "$site" 2>/dev/null || true
    [ -n "$cluster" ] && kill "$cluster" 2>/dev/null || true
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

npm run -s lint
npm run -s cluster >"$dir/cluster.log" 2>&1 &
cluster=$!
# Up once the router has its nodes, which it prints.
i=0
until grep -q '^router ' "$dir/cluster.log" 2>/dev/null; do
    i=$((i + 1))
    if [ $i -gt 600 ]; then cat "$dir/cluster.log"; exit 1; fi
    sleep 0.1
done
npm run -s setup
npm run -s build >"$dir/build.log" 2>&1 || { cat "$dir/build.log"; exit 1; }
PORT=$site_port npm run -s start >"$dir/site.log" 2>&1 &
site=$!
wait_for "$PLOVER_SITE/signin" "$dir/site.log"
npm test
if [ -n "${DISPATCH_BENCH:-}" ]; then npm run -s bench; fi
