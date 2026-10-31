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
# The Quickstart's programs (quickstart/) are the files its fourth step
# shows, each whole, and each is run against a server holding the notes of
# its first steps: what it prints is what the page shows under the tabs.
# Last, docs_check.py holds the docs to these programs: the statements of
# each tab on Your language are its program's, and the HTTP endpoint page's
# curl tab and the Quickstart's curl steps are run as written.
# Each language gets a server of its own over a new file, so every program
# starts from an empty database and makes the same collection, on a port
# the system picks: a run never meets another, or any other server, on one.
# Under CI a missing toolchain fails the run rather than skip it.
#
#   integrations/languages/run-tests.sh
#   integrations/languages/run-tests.sh quickstart   # the Quickstart's programs alone
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
# endpoint page's curl commands (which send none) are written for. `insecure`
# is the Quickstart's with a container in reach: no token, on every address.
serve() {
    rm -f "$dir/server.log"
    if [ "${2:-}" = open ]; then
        "$root/target/debug/fenec-server" --file "$dir/$1.fenec" \
            --http 127.0.0.1:0 2>"$dir/server.log" &
    elif [ "${2:-}" = insecure ]; then
        "$root/target/debug/fenec-server" --file "$dir/$1.fenec" \
            --http 0.0.0.0:0 --insecure 2>"$dir/server.log" &
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

# The Quickstart's programs: a copy of a language's directory with the address
# its files ask (the Quickstart's own, 127.0.0.1:8080) made the server's, and
# the SDKs' paths (../../../, the checkout's integrations/) made absolute, then
# the program run with the notes of the page's steps 2 and 3 on the server, and
# what it printed held to the block the page shows (docs_check.py).
qs_copy() {
    rm -rf "$dir/qs-$1"
    cp -R "$here/quickstart/$1" "$dir/qs-$1"
    find "$dir/qs-$1" -type f | while read -r f; do
        sed -e "s|http://127.0.0.1:8080|$2|g" -e "s|\.\./\.\./\.\./|$root/integrations/|g" "$f" > "$f.new"
        mv "$f.new" "$f"
    done
}

qs_prints() {
    printf '%s\n' "$1" > "$dir/out"
    python3 "$here/docs_check.py" prints "$dir/out"
    echo "quickstart $2: ok"
}

quickstart_tests() {
    if ! command -v python3 >/dev/null 2>&1; then
        missing python3 "the Quickstart's programs"
        return 0
    fi
    python3 "$here/docs_check.py" programs

    for lang in python java php ruby; do
        serve "quickstart-$lang" insecure
        python3 "$here/docs_check.py" seed "$local_url"
        qs_copy "$lang" "http://host.docker.internal:$port"
        case $lang in
            python) out=$(container python:3.13-slim "$dir/qs-python" \
                "cp -r /fenecdb /tmp/fenecdb &&
                 pip install -q --root-user-action=ignore --disable-pip-version-check /tmp/fenecdb &&
                 python3 notes.py") ;;
            java) out=$(container eclipse-temurin:21 "$dir/qs-java" "java Notes.java") ;;
            php) out=$(container php:8.3-cli "$dir/qs-php" "php notes.php") ;;
            ruby) out=$(container ruby:3.3 "$dir/qs-ruby" "ruby notes.rb") ;;
        esac
        stop
        qs_prints "$out" "$lang"
    done

    if command -v node >/dev/null 2>&1; then
        serve quickstart-javascript insecure
        python3 "$here/docs_check.py" seed "$local_url"
        qs_copy javascript "$local_url"
        out=$(node "$dir/qs-javascript/notes.mjs")
        stop
        qs_prints "$out" javascript
    else
        missing node "the Quickstart's JavaScript"
    fi

    if command -v go >/dev/null 2>&1; then
        serve quickstart-go insecure
        python3 "$here/docs_check.py" seed "$local_url"
        qs_copy go "$local_url"
        out=$(cd "$dir/qs-go" && go run .)
        stop
        qs_prints "$out" go
    else
        missing go "the Quickstart's Go"
    fi

    if command -v dotnet >/dev/null 2>&1; then
        serve quickstart-dotnet insecure
        python3 "$here/docs_check.py" seed "$local_url"
        qs_copy dotnet "$local_url"
        out=$(cd "$dir/qs-dotnet" && dotnet run -c Release)
        stop
        qs_prints "$out" dotnet
    else
        missing dotnet "the Quickstart's .NET"
    fi

    serve quickstart-rust insecure
    python3 "$here/docs_check.py" seed "$local_url"
    qs_copy rust "$local_url"
    out=$(cd "$dir/qs-rust" && "$cargo" run -q)
    stop
    qs_prints "$out" rust
}

if [ "${1:-}" = quickstart ]; then
    quickstart_tests
    exit 0
fi

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

quickstart_tests
