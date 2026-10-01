#!/bin/sh
# Each language's example on the docs' Your language page, run against a
# real fenec-server over HTTP: Python (the fenecdb client), Java
# (java.net.http), PHP (curl) and Ruby (Net::HTTP) from containers of their
# own, as integrations/python runs its stores -- a runner need not carry the
# toolchains, and every machine runs the same -- then JavaScript (fetch, and
# @fenecdb/web's client) with the Node, Go (net/http) with the Go, C#
# (HttpClient) with the .NET and Rust (ureq) with the Rust on the machine.
# Each language gets a server of its own over a new file, so every program
# starts from an empty database and makes the same collection. Under CI a
# missing toolchain fails the run rather than skip it.
#
#   integrations/languages/run-tests.sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
cargo=${CARGO:-cargo}
[ -x "$HOME/.cargo/bin/cargo" ] && cargo="$HOME/.cargo/bin/cargo"
"$cargo" build -q -p fenec-server --manifest-path "$root/Cargo.toml"

dir=$(mktemp -d)
port=${FENEC_TEST_PORT:-18182}
token=language-tests
server=
trap 'stop; rm -rf "$dir"' EXIT INT TERM

# Outside loopback, so a container reaches it: a token is required there.
serve() {
    rm -f "$dir/server.log"
    "$root/target/debug/fenec-server" --file "$dir/$1.fenec" \
        --http "0.0.0.0:$port" --http-token "$token" 2>"$dir/server.log" &
    server=$!
    i=0
    until grep -q "listening on: http://" "$dir/server.log" 2>/dev/null; do
        i=$((i + 1))
        [ $i -gt 100 ] && { cat "$dir/server.log"; exit 1; }
        sleep 0.1
    done
}

stop() {
    [ -n "$server" ] || return 0
    kill "$server" 2>/dev/null || true
    wait "$server" 2>/dev/null || true
    server=
}

missing() {
    echo "$1 not found -- $2 skipped"
    [ -z "${CI:-}" ]
}

# host.docker.internal is Docker Desktop's name for the machine; a Linux
# daemon, a CI runner's, only knows it when told.
container() {
    image=$1
    src=$2
    shift 2
    docker run --rm -v "$src:/src:ro" -v "$root/integrations/python:/fenecdb:ro" \
        --add-host=host.docker.internal:host-gateway \
        -e FENEC_URL="http://host.docker.internal:$port" -e FENEC_TOKEN="$token" \
        "$image" sh -c "cp -r /src /work && cd /work && $*"
}

local_url="http://127.0.0.1:$port"

serve python
container python:3.13-slim "$here/python" \
    "cp -r /fenecdb /tmp/fenecdb &&
     pip install -q --root-user-action=ignore --disable-pip-version-check /tmp/fenecdb &&
     python search.py"
stop

serve java
container eclipse-temurin:21 "$here/java" "java Search.java"
stop

serve php
container php:8.3-cli "$here/php" "php search.php"
stop

serve ruby
container ruby:3.3 "$here/ruby" "ruby search.rb"
stop

if command -v node >/dev/null 2>&1; then
    serve javascript
    FENEC_URL="$local_url" FENEC_TOKEN="$token" node "$here/javascript/search.mjs"
    stop
    serve javascript-client
    FENEC_URL="$local_url" FENEC_TOKEN="$token" node "$here/javascript/client.mjs"
    stop
else
    missing node "JavaScript's example"
fi

if command -v go >/dev/null 2>&1; then
    serve go
    (cd "$here/go" && FENEC_URL="$local_url" FENEC_TOKEN="$token" go run .)
    stop
else
    missing go "Go's example"
fi

if command -v dotnet >/dev/null 2>&1; then
    serve dotnet
    (cd "$here/dotnet" && FENEC_URL="$local_url" FENEC_TOKEN="$token" \
        dotnet run -c Release)
    stop
else
    missing dotnet ".NET's example"
fi

serve rust
(cd "$here/rust" && FENEC_URL="$local_url" FENEC_TOKEN="$token" "$cargo" run -q)
stop
