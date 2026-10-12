#!/bin/sh
# Kestrel end to end, as CI runs it: a tenant node over a new directory
# (scripts/db.sh), the tenants made and 40 days of demo history written and
# folded, lint, the browser's files built, Kestrel started, then the tests
# -- correctness, security, SEO and the latency budgets.
#
# The node and Kestrel each listen on a port the system picks and say
# which, so two runs -- or this beside any other suite or server -- never
# meet on one.
#
#   FENEC_SERVER              the server binary (scripts/db.sh finds one otherwise)
#   ANALYTICS_LIGHTHOUSE=1    Lighthouse CI's budgets too (lighthouserc.cjs)
#   ANALYTICS_BENCH=1         npm run bench:ingest and bench:queries as well
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"
dir=$(mktemp -d)
export FENEC_PORT=0
export FENEC_DIR="$dir/tenants"
export FENEC_AUDIT="$dir/audit.log"
export KESTREL_INSECURE_COOKIES=1 # plain http on localhost
db= app=
stop() {
    [ -n "$app" ] && kill "$app" 2>/dev/null || true
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

sh scripts/db.sh >"$dir/db.log" 2>&1 &
db=$!
until_line 'listening on: http://[^ ]+ ' "$dir/db.log" "$db"
FENEC_URL=$(sed -n 's/.*listening on: \(http:[^ ]*\) .*/\1/p' "$dir/db.log" | head -1)
export FENEC_URL
wait_for "$FENEC_URL/_health" "$dir/db.log"
KESTREL_SEED_DAYS=${KESTREL_SEED_DAYS:-40} npm run -s setup
KESTREL_ROLLUP_ONCE=1 npx tsx scripts/rollup.ts
npm run -s lint
npm run -s build
PORT=0 KESTREL_ROLLUP_SITES=fieldnotes,tidepool npm run -s start >"$dir/app.log" 2>&1 &
app=$!
until_line '^Kestrel on http://' "$dir/app.log" "$app"
KESTREL_URL=$(sed -n 's/^Kestrel on \(http:[^ ]*\).*/\1/p' "$dir/app.log" | head -1)
export KESTREL_URL
wait_for "$KESTREL_URL/robots.txt" "$dir/app.log"
npm test
if [ -n "${ANALYTICS_LIGHTHOUSE:-}" ]; then sh scripts/lighthouse.sh; fi
if [ -n "${ANALYTICS_BENCH:-}" ]; then npm run -s bench:ingest && npm run -s bench:queries; fi
