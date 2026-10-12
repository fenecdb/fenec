#!/bin/sh
# The Python integrations against a real fenec-server: built here, started on
# this machine, and tested from a python:3.13 container, which gets the same
# interpreter everywhere -- a Homebrew Python whose pyexpat cannot load is
# how this came to use one.
#
#   integrations/python/run-tests.sh [pytest arguments]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
cargo=${CARGO:-cargo}
[ -x "$HOME/.cargo/bin/cargo" ] && cargo="$HOME/.cargo/bin/cargo"
"$cargo" build -q -p fenec-server --manifest-path "$root/Cargo.toml"

dir=$(mktemp -d)
token=python-tests
# Outside loopback, so the container reaches it: a token is required there.
# On a port the system picks, read off the server's `listening on:` line: a
# run never meets another, or any other server, on one.
"$root/target/debug/fenec-server" --file "$dir/python.fenec" \
    --http 0.0.0.0:0 --http-token "$token" --cdc 2>"$dir/server.log" &
server=$!
trap 'kill $server 2>/dev/null; rm -rf "$dir"' EXIT INT TERM
i=0
until grep -q "listening on: http://.* " "$dir/server.log"; do
    i=$((i + 1))
    [ $i -gt 100 ] && { cat "$dir/server.log"; exit 1; }
    sleep 0.1
done
port=$(sed -n 's/.*listening on: http:[^ ]*:\([0-9]*\) .*/\1/p' "$dir/server.log" | head -1)

# The source is copied inside, so the build and pytest's caches stay there.
# host.docker.internal is Docker Desktop's name for the machine; a Linux
# daemon, a CI runner's, only knows it when told. The builder's golden file
# lies outside the package, and goes in beside it.
docker run --rm -v "$here:/src:ro" --add-host=host.docker.internal:host-gateway \
    -v "$root/integrations/builder-golden.json:/golden.json:ro" -e FENEC_GOLDEN=/golden.json \
    -e FENEC_URL="http://host.docker.internal:$port" -e FENEC_TOKEN="$token" \
    python:3.13-slim sh -c \
    "cp -r /src /work && cd /work &&
     pip install -q --root-user-action=ignore --disable-pip-version-check '.[test]' &&
     python -m pytest -q -p no:cacheprovider $*"
