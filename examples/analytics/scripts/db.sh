#!/bin/sh
# fenec-server for Kestrel: a tenant node (a file per site in data/tenants),
# the node's own token, JWTs checked against policy.txt and bound to their
# tenant, the change stream kept for the rollup worker, and an audit log.
#
#   FENEC_SERVER   the binary (default: this repository's target/release, then target/debug)
#   FENEC_PORT     8080
#   FENEC_DIR      data/tenants
#   FENEC_SYNC     250: a write is on disk within 250 ms. Events are
#                  counted, not money; `always` costs ingest an fsync a batch
#   FENEC_TOKEN, FENEC_ADMIN_TOKEN, FENEC_JWT_SECRET   as src/config.ts reads them
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
printf '%s' "${FENEC_JWT_SECRET:-kestrel-dev-jwt-secret-of-at-least-32-bytes}" >"$secret"
# --replication-token keeps each tenant's change stream (/t/<site>/_changes)
# for the rollup worker; no replica need follow it. --replication-buffer is
# how far behind the worker may fall before it rebuilds from the raw events.
# --warm builds a site's indexes as it opens rather than in the first
# dashboard request after a restart.
FENEC_HTTP_TOKEN=${FENEC_TOKEN:-kestrel-dev-operator} exec "$server" \
    --dir "$dir" \
    --http "127.0.0.1:${FENEC_PORT:-8080}" \
    --admin-token "${FENEC_ADMIN_TOKEN:-kestrel-dev-node-admin}" \
    --replication-token "${FENEC_REPLICATION_TOKEN:-kestrel-dev-replication}" \
    --replication-buffer "${FENEC_REPLICATION_BUFFER:-256}" \
    --jwt-secret-file "$secret" \
    --policy "$here/policy.txt" \
    --audit "${FENEC_AUDIT:-$dir/../audit.log}" \
    --warm all \
    --sync "${FENEC_SYNC:-250}"
