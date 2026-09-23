#!/bin/sh
# The Python integrations against a real fenec-pg: built here, started on
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
"$cargo" build -q -p fenec-pg --manifest-path "$root/Cargo.toml"

dir=$(mktemp -d)
port=${FENEC_TEST_PORT:-18181}
token=python-tests
# Outside loopback, so the container reaches it: a token is required there.
"$root/target/debug/fenec-pg" --listen 127.0.0.1:0 --file "$dir/python.fenec" \
    --http "0.0.0.0:$port" --http-token "$token" 2>"$dir/server.log" &
server=$!
trap 'kill $server 2>/dev/null; rm -rf "$dir"' EXIT INT TERM
i=0
until grep -q "listening on: http://" "$dir/server.log"; do
    i=$((i + 1))
    [ $i -gt 100 ] && { cat "$dir/server.log"; exit 1; }
    sleep 0.1
done

# The source is copied inside, so the build and pytest's caches stay there.
docker run --rm -v "$here:/src:ro" \
    -e FENEC_URL="http://host.docker.internal:$port" -e FENEC_TOKEN="$token" \
    python:3.13-slim sh -c \
    "cp -r /src /work && cd /work &&
     pip install -q --root-user-action=ignore --disable-pip-version-check '.[test]' &&
     python -m pytest -q -p no:cacheprovider $*"
