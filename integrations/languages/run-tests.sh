#!/bin/sh
# Each language's example on the docs' Your language page, run against a
# real fenec-server over HTTP: Python (the fenecdb client), Java
# (java.net.http), PHP (curl) and Ruby (Net::HTTP) from containers of their
# own, as integrations/python runs its stores -- a runner need not carry the
# toolchains, and every machine runs the same -- then JavaScript (fetch, and
# @fenecdb/web's client) with the Node, Go (net/http, and the fenecdb SDK
# in integrations/go) with the Go, C# (HttpClient, and the FenecDb SDK in
# integrations/dotnet) with the .NET and Rust (ureq) with the Rust on the
# machine.
# Last, docs_check.py holds the docs to these programs: the statements of
# each tab on Your language are its program's, and the HTTP endpoint page's
# curl tab and the Quickstart's curl steps are run as written.
# Each language gets a server of its own over a new file, so every program
# starts from an empty database and makes the same collection, on a port
# the system picks: a run never meets another, or any other server, on one.
# Under CI a missing toolchain fails the run rather than skip it.
#
#   integrations/languages/run-tests.sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
cargo=${CARGO:-cargo}
[ -x "$HOME/.cargo/bin/cargo" ] && cargo="$HOME/.cargo/bin/cargo"
"$cargo" build -q -p fenec-server --manifest-path "$root/Cargo.toml"

dir=$(mktemp -d)
port=
token=language-tests
server=
trap 'stop; rm -rf "$dir"' EXIT INT TERM

# Outside loopback, so a container reaches it: a token is required there.
# On :0, the port read off the server's `listening on: http://<address>` line.
# `serve <name> open` is the docs' own: loopback and no token, as the HTTP
# endpoint page's curl commands (which send none) are written for.
serve() {
    rm -f "$dir/server.log"
    if [ "${2:-}" = open ]; then
        "$root/target/debug/fenec-server" --file "$dir/$1.fenec" \
            --http 127.0.0.1:0 2>"$dir/server.log" &
    else
        "$root/target/debug/fenec-server" --file "$dir/$1.fenec" \
            --http 0.0.0.0:0 --http-token "$token" 2>"$dir/server.log" &
    fi
    server=$!
    i=0
    until grep -q "listening on: http://.* " "$dir/server.log" 2>/dev/null; do
        i=$((i + 1))
        [ $i -gt 100 ] && { cat "$dir/server.log"; exit 1; }
        sleep 0.1
    done
    port=$(sed -n 's/.*listening on: http:[^ ]*:\([0-9]*\) .*/\1/p' "$dir/server.log" | head -1)
    local_url="http://127.0.0.1:$port"
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
    serve go-sdk
    (cd "$here/go-sdk" && FENEC_URL="$local_url" FENEC_TOKEN="$token" go run .)
    stop
else
    missing go "Go's example"
fi

if command -v dotnet >/dev/null 2>&1; then
    serve dotnet
    (cd "$here/dotnet" && FENEC_URL="$local_url" FENEC_TOKEN="$token" \
        dotnet run -c Release)
    stop
    serve dotnet-sdk
    (cd "$here/dotnet-sdk" && FENEC_URL="$local_url" FENEC_TOKEN="$token" \
        dotnet run -c Release)
    stop
else
    missing dotnet ".NET's example"
fi

serve rust
(cd "$here/rust" && FENEC_URL="$local_url" FENEC_TOKEN="$token" "$cargo" run -q)
stop

# The docs against these programs: the statements each HTTP tab of Your
# language sends are the ones its program does, and the HTTP endpoint page's
# curl tab and the Quickstart's curl steps are run as written, against a
# server with no token and a new file (docs_check.py).
if command -v python3 >/dev/null 2>&1; then
    serve docs open
    python3 "$here/docs_check.py" "$local_url"
    stop
else
    missing python3 "the docs' check"
fi
