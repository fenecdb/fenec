#!/bin/sh
# PostgreSQL drivers against a real fenec-pg's pg wire: psycopg and
# SQLAlchemy, from a python:3.13 container, as integrations/python runs the
# stores. What a driver sends on its own -- a savepoint for a nested
# transaction, the queries a dialect opens a connection with -- is what the
# server is held to here.
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
    "cp -r /src /work && cd /work &&
     pip install -q --root-user-action=ignore --disable-pip-version-check -r requirements.txt &&
     python -m pytest -q -p no:cacheprovider $*"
