#!/bin/sh
# The ledger end to end, as CI runs it: a tenant node over a new directory
# (scripts/db.sh: --sync always, the policy, the audit log), the tenants
# made, lint, then the tests -- invariants in process and over HTTP,
# security, the change-stream sink, and a crash under kill -9 against a
# server of the test's own. LEDGER_BENCH=1 adds the measurements.
#
# The node listens on a port the system picks, so two runs -- or this
# beside any other suite or server -- never meet on one.
#
#   FENEC_SERVER     the server binary (scripts/db.sh finds one otherwise)
#   LEDGER_BENCH=1   npm run bench and npm run recon-bench as well
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"
dir=$(mktemp -d)
export FENEC_PORT=0
export FENEC_DIR="$dir/tenants"
export FENEC_AUDIT="$dir/audit.log"
db=
stop() {
    [ -n "$db" ] && kill "$db" 2>/dev/null || true
    wait 2>/dev/null || true
    rm -rf "$dir"
}
trap stop EXIT INT TERM

sh scripts/db.sh >"$dir/db.log" 2>&1 &
db=$!
# Up once it says where it listens: the url is read off its line.
i=0
until grep -Eq 'listening on: http://[^ ]+ ' "$dir/db.log" 2>/dev/null; do
    i=$((i + 1))
    if [ $i -gt 600 ] || ! kill -0 "$db" 2>/dev/null; then cat "$dir/db.log"; exit 1; fi
    sleep 0.1
done
FENEC_URL=$(sed -n 's/.*listening on: \(http:[^ ]*\) .*/\1/p' "$dir/db.log" | head -1)
export FENEC_URL
npm run -s setup
npm run -s lint
npm test
if [ -n "${LEDGER_BENCH:-}" ]; then npm run -s bench && npm run -s recon-bench; fi
