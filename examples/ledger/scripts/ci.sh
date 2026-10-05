#!/bin/sh
# The ledger end to end, as CI runs it: a tenant node over a new directory
# (scripts/db.sh: --sync always, the policy, the audit log), the tenants
# made, lint, then the tests -- invariants in process and over HTTP,
# security, the change-stream sink, and a crash under kill -9 against a
# server of the test's own. LEDGER_BENCH=1 adds the measurements.
#
#   FENEC_SERVER     the server binary (scripts/db.sh finds one otherwise)
#   LEDGER_BENCH=1   npm run bench and npm run recon-bench as well
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"
dir=$(mktemp -d)
port=${FENEC_PORT:-18390}
export FENEC_URL="http://127.0.0.1:$port"
export FENEC_PORT=$port
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
i=0
until curl -s -o /dev/null "$FENEC_URL/_health"; do
    i=$((i + 1))
    if [ $i -gt 600 ]; then cat "$dir/db.log"; exit 1; fi
    sleep 0.1
done
npm run -s setup
npm run -s lint
npm test
if [ -n "${LEDGER_BENCH:-}" ]; then npm run -s bench && npm run -s recon-bench; fi
