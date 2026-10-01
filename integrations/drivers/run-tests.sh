#!/bin/sh
# PostgreSQL drivers against a real fenec-pg's pg wire: psycopg, asyncpg
# and SQLAlchemy from a python:3.13 container, as integrations/python runs
# the stores, then pgx with the Go, node-postgres with the Node, Npgsql with
# the .NET and tokio-postgres with the Rust on the machine -- each with pgvector's
# library for it. What a driver sends on its own -- a savepoint for a nested
# transaction, the queries a dialect opens a connection with, the rows of a
# COPY, the binary format it asks rows in -- is what the server is held to
# here. Under CI a missing toolchain fails the run rather than skip it.
#
#   integrations/drivers/run-tests.sh [pytest arguments]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
cargo=${CARGO:-cargo}
[ -x "$HOME/.cargo/bin/cargo" ] && cargo="$HOME/.cargo/bin/cargo"
"$cargo" build -q -p fenec-pg --manifest-path "$root/Cargo.toml"

dir=$(mktemp -d)
port=${FENEC_TEST_PG_PORT:-18182}
password=driver-tests
# Outside loopback, so the container reaches it: a password is required there.
"$root/target/debug/fenec-pg" --listen "0.0.0.0:$port" --file "$dir/drivers.fenec" \
    --password "$password" 2>"$dir/server.log" &
server=$!
trap 'kill $server 2>/dev/null; rm -rf "$dir"' EXIT INT TERM
i=0
until grep -q "listening on: postgres://" "$dir/server.log"; do
    i=$((i + 1))
    [ $i -gt 100 ] && { cat "$dir/server.log"; exit 1; }
    sleep 0.1
done

docker run --rm -v "$here:/src:ro" --add-host=host.docker.internal:host-gateway \
    -e FENEC_PG="host=host.docker.internal port=$port user=fenec password=$password dbname=fenec" \
    python:3.13-slim sh -c \
    "mkdir /work && cp /src/*.py /src/requirements.txt /work && cd /work &&
     pip install -q --root-user-action=ignore --disable-pip-version-check -r requirements.txt &&
     python -m pytest -q -p no:cacheprovider $*"

url="postgres://fenec:$password@127.0.0.1:$port/fenec"
missing() {
    echo "$1 not found -- $2 skipped"
    [ -z "${CI:-}" ]
}
if command -v go >/dev/null 2>&1; then
    (cd "$here/go" && FENEC_PG_URL="$url" go test -count=1 ./...)
else
    missing go "pgx's tests"
fi
if command -v node >/dev/null 2>&1; then
    (cd "$here/node" && npm ci --no-audit --no-fund --silent && FENEC_PG_URL="$url" node --test)
else
    missing node "node-postgres's tests"
fi
if command -v dotnet >/dev/null 2>&1; then
    (cd "$here/dotnet" &&
        FENEC_PG_NPGSQL="Host=127.0.0.1;Port=$port;Username=fenec;Password=$password;Database=fenec" \
        dotnet run -c Release)
else
    missing dotnet "Npgsql's tests"
fi
(cd "$here/rust" &&
    FENEC_PG="host=127.0.0.1 port=$port user=fenec password=$password dbname=fenec" \
    "$cargo" test -q)
