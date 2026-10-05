#!/bin/sh
# fenec-server for the shop: the file in data/, the shop's token, shoppers'
# tokens checked against policy.txt, and the browser's live stock let in
# from the site's origin. Every index is built after the open, beside the
# first requests (`--warm`, the default for a file, written out here): the
# first search page after a start took 2.68 s to its largest paint while
# its text index was built inside it, against 1.40 s warm.
#
#   FENEC_SERVER   the binary (default: this repository's target/release, then target/debug)
#   FENEC_PORT     8080
#   FENEC_FILE     data/shop.fenec
#   FENEC_TOKEN, FENEC_JWT_SECRET, SITE_URL   as lib/db.ts and lib/jwt.ts read them
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
root=$(cd "$here/../.." && pwd)
server=${FENEC_SERVER:-}
if [ -z "$server" ]; then
    for b in "$root/target/release/fenec-server" "$root/target/debug/fenec-server"; do
        if [ -x "$b" ]; then server=$b; break; fi
    done
fi
[ -n "$server" ] || { echo "no fenec-server: cargo build --release -p fenec-server, or set FENEC_SERVER" >&2; exit 1; }
mkdir -p "$here/data"
umask 077
printf '%s' "${FENEC_JWT_SECRET:-shop-dev-jwt-secret-of-at-least-32-bytes!}" >"$here/data/jwt.secret"
FENEC_HTTP_TOKEN=${FENEC_TOKEN:-shop-dev-token} exec "$server" \
    --file "${FENEC_FILE:-$here/data/shop.fenec}" \
    --http "127.0.0.1:${FENEC_PORT:-8080}" \
    --jwt-secret-file "$here/data/jwt.secret" \
    --policy "$here/policy.txt" \
    --http-cors "${SITE_URL:-http://localhost:3000}" \
    --sync "${FENEC_SYNC:-250}" \
    --warm "${FENEC_WARM:-all}"
