#!/bin/sh
# fenec-server for the ledger: a tenant node (a file per tenant in
# data/tenants), the operator's token, the node's admin token, the
# console's and customers' tokens checked against policy.txt, every write
# on disk before it is answered, the change stream kept for the sink, and
# an audit log of refused tokens and schema changes.
#
#   FENEC_SERVER   the binary (default: this repository's target/release, then target/debug)
#   FENEC_PORT     8080
#   FENEC_DIR      data/tenants
#   FENEC_SYNC     always
#   FENEC_TOKEN, FENEC_ADMIN_TOKEN, FENEC_JWT_SECRET   as src/config.ts and src/tokens.ts read them
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
dir=${FENEC_DIR:-$here/data/tenants}
mkdir -p "$dir"
umask 077
secret="$dir/../jwt.secret"
printf '%s' "${FENEC_JWT_SECRET:-ledger-dev-jwt-secret-of-at-least-32-bytes}" >"$secret"
# --replication-token keeps each tenant's change stream (/t/<t>/_changes);
# no replica need follow it.
FENEC_HTTP_TOKEN=${FENEC_TOKEN:-ledger-dev-operator} exec "$server" \
    --dir "$dir" \
    --http "127.0.0.1:${FENEC_PORT:-8080}" \
    --admin-token "${FENEC_ADMIN_TOKEN:-ledger-dev-node-admin}" \
    --replication-token "${FENEC_REPLICATION_TOKEN:-ledger-dev-replication}" \
    --jwt-secret-file "$secret" \
    --policy "$here/policy.txt" \
    --audit "${FENEC_AUDIT:-$dir/../audit.log}" \
    --sync "${FENEC_SYNC:-always}"
