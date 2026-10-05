#!/bin/sh
# The Notes examples against this repository's own build (`make
# examples-test`): each example's `smoke` -- create, search, filter, a live
# update where the platform has one, a reopen where the engine is local --
# run on the packages built here rather than the published ones. A server
# example gets a fenec-server of its own over a new file.
#
#   examples/run-tests.sh                  every example whose toolchain is here
#   examples/run-tests.sh python go rust   those alone
#
# Under CI a missing toolchain fails the run rather than skip it.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/.." && pwd)
cargo=${CARGO:-cargo}
[ -x "$HOME/.cargo/bin/cargo" ] && cargo="$HOME/.cargo/bin/cargo"

dir=$(mktemp -d)
port=${FENEC_TEST_PORT:-18190}
url="http://127.0.0.1:$port"
token=examples-test
server=
trap 'stop; rm -rf "$dir"' EXIT INT TERM

serve() {
    rm -f "$dir/server.log"
    "$root/target/debug/fenec-server" --file "$dir/$1.fenec" --http "127.0.0.1:$port" \
        --http-token "$token" --cdc 2>"$dir/server.log" &
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

has() { command -v "$1" >/dev/null 2>&1; }

# An example copied out of the tree, so that what a run installs or writes
# stays out of it: node_modules, a go.mod's replace, a build directory.
copy() {
    rm -rf "$dir/$1"
    mkdir -p "$dir/$1"
    (cd "$here/$1" && tar cf - --exclude node_modules --exclude dist --exclude target \
        --exclude bin --exclude obj --exclude .build --exclude build --exclude .next \
        --exclude data --exclude .lighthouseci .) | (cd "$dir/$1" && tar xf -)
    echo "$dir/$1"
}

# @fenecdb/web and @fenecdb/react as npm would publish them, packed here.
packed() {
    [ -f "$dir/packs/done" ] && return 0
    # To stderr: what an npm example prints on stdout is its directory.
    [ -f "$root/web/fenec.wasm" ] || (cd "$root" && make -s wasm) >&2
    mkdir -p "$dir/packs"
    (cd "$root/web" && npm pack -q --pack-destination "$dir/packs" >/dev/null)
    (cd "$root/integrations/react" && npm pack -q --pack-destination "$dir/packs" >/dev/null)
    touch "$dir/packs/done"
}

# An npm example on the packed packages: its dependencies pointed at them.
npm_example() {
    packed
    w=$(copy "$1")
    (cd "$w" &&
        npm pkg set "dependencies.@fenecdb/web=file:$(ls "$dir"/packs/fenecdb-web-*.tgz)" &&
        if grep -q '"@fenecdb/react"' package.json; then
            npm pkg set "dependencies.@fenecdb/react=file:$(ls "$dir"/packs/fenecdb-react-*.tgz)"
        fi &&
        npm install -q --no-audit --no-fund --loglevel=error) >&2
    echo "$w"
}

run_node_server() {
    has node || { missing node "node-server"; return; }
    w=$(npm_example node-server)
    serve node-server
    (cd "$w" && FENEC_URL="$url" FENEC_TOKEN="$token" npm run -s smoke)
    stop
}

run_web_local() {
    has node || { missing node "web-local"; return; }
    w=$(npm_example web-local)
    (cd "$w" && npm run -s smoke && npm run -s build)
}

run_react() {
    has node || { missing node "react"; return; }
    w=$(npm_example react)
    serve react
    (cd "$w" && FENEC_URL="$url" FENEC_TOKEN="$token" npm run -s smoke && npm run -s build)
    stop
    # The one line switched to the synced replica: the components still type-check.
    sed -i.bak -e 's#^export const db = await local#// &#' -e 's#^// \(export const db = await synced\)#\1#' "$w/src/db.ts"
    grep -q '^export const db = await synced' "$w/src/db.ts"
    (cd "$w" && npx tsc --noEmit)
}

# The shop (examples/shop/scripts/ci.sh): a server and a Next.js site of its
# own, the catalog seeded, lint and its correctness, security and SEO tests;
# SHOP_LIGHTHOUSE=1 adds Lighthouse CI, SHOP_LOAD=1 the timings and load test.
run_shop() {
    has node || { missing node "shop"; return; }
    w=$(npm_example shop)
    (cd "$w" && FENEC_SERVER="${SHOP_FENEC_SERVER:-$root/target/debug/fenec-server}" \
        FENEC_PORT=$port SHOP_PORT=$((port + 1)) sh scripts/ci.sh)
}

# The ledger (examples/ledger/scripts/ci.sh): a tenant node of its own over a
# new directory, lint, and its invariant, security, sink and crash tests;
# LEDGER_BENCH=1 adds the measurements.
run_ledger() {
    has node || { missing node "ledger"; return; }
    w=$(npm_example ledger)
    [ -x "${LEDGER_FENEC_CLI:-$root/target/debug/fenec}" ] || "$cargo" build -q -p fenec-cli --manifest-path "$root/Cargo.toml"
    (cd "$w" && FENEC_SERVER="${LEDGER_FENEC_SERVER:-$root/target/debug/fenec-server}" \
        FENEC_CLI="${LEDGER_FENEC_CLI:-$root/target/debug/fenec}" \
        FENEC_PORT=$((port + 2)) sh scripts/ci.sh)
}

# Trellis (examples/saas/scripts/ci.sh): lint, and its auth, token, role,
# realtime, search and operations tests, each file over a cluster of its
# own -- three tenant nodes and fenec-shard; SAAS_BENCH=1 adds the
# measurements.
run_saas() {
    has node || { missing node "saas"; return; }
    w=$(npm_example saas)
    [ -x "${SAAS_FENEC_SHARD:-$root/target/debug/fenec-shard}" ] || "$cargo" build -q -p fenec-shard --manifest-path "$root/Cargo.toml"
    (cd "$w" && FENEC_SERVER="${SAAS_FENEC_SERVER:-$root/target/debug/fenec-server}" \
        FENEC_SHARD="${SAAS_FENEC_SHARD:-$root/target/debug/fenec-shard}" sh scripts/ci.sh)
}

# Kestrel (examples/analytics/scripts/ci.sh): a tenant node and the app of
# its own, 40 days of history folded, lint and its correctness, security,
# SEO and latency tests; ANALYTICS_LIGHTHOUSE=1 adds Lighthouse CI,
# ANALYTICS_BENCH=1 the measurements.
run_analytics() {
    has node || { missing node "analytics"; return; }
    w=$(npm_example analytics)
    (cd "$w" && FENEC_SERVER="${ANALYTICS_FENEC_SERVER:-$root/target/debug/fenec-server}" \
        FENEC_PORT=$((port + 4)) KESTREL_PORT=$((port + 5)) sh scripts/ci.sh)
}

run_python() {
    has python3 || { missing python3 "python"; return; }
    serve python
    (cd "$here/python" && PYTHONPATH="$root/integrations/python" FENEC_URL="$url" FENEC_TOKEN="$token" \
        python3 notes.py smoke)
    stop
}

run_go() {
    has go || { missing go "go"; return; }
    w=$(copy go)
    (cd "$w" && go mod edit -replace "github.com/fenecdb/fenec/integrations/go=$root/integrations/go" && go vet ./...)
    serve go
    (cd "$w" && FENEC_URL="$url" FENEC_TOKEN="$token" go run . smoke)
    stop
}

run_dotnet() {
    has dotnet || { missing dotnet "dotnet"; return; }
    w=$(copy dotnet)
    (cd "$w" && dotnet build -v q -nologo -p:FenecLocal="$root" >/dev/null)
    serve dotnet
    (cd "$w" && FENEC_URL="$url" FENEC_TOKEN="$token" dotnet run --no-build -p:FenecLocal="$root" -- smoke)
    stop
}

run_rust() {
    # The tag's crates become this checkout's: a [patch] of a git source
    # still fetches the tag, which a release names before it is pushed.
    w=$(copy rust)
    sed -E "s#^(fenec-[a-z]+) = \{ git = \"[^\"]+\", tag = \"[^\"]+\"#\1 = { path = \"$root/crates/\1\"#" \
        "$here/rust/Cargo.toml" >"$w/Cargo.toml"
    (cd "$w" && "$cargo" run -q --target-dir "$root/target/examples" -- smoke)
}

run_swift() {
    [ "$(uname)" = Darwin ] && has swift || { missing swift "swift"; return; }
    [ -d "$root/integrations/swift/build/FenecFFI.xcframework" ] || "$root/integrations/swift/build-xcframework.sh" --macos
    (cd "$here/swift" && FENEC_LOCAL=1 swift build -q && FENEC_LOCAL=1 swift run -q notes-cli smoke)
}

run_kotlin_android() {
    "$here/kotlin-android/run-smoke.sh"
}

# Flutter's own script: flutter test where Flutter is, the notes' logic
# under plain Dart where only Dart is.
run_flutter() {
    (cd "$here/flutter" && ./run-tests.sh)
}

all="node-server web-local react shop ledger saas analytics python go dotnet rust swift kotlin-android flutter"
"$cargo" build -q -p fenec-server --manifest-path "$root/Cargo.toml"
for e in ${*:-$all}; do
    echo "== $e"
    case $e in
        node-server) run_node_server ;;
        web-local) run_web_local ;;
        react) run_react ;;
        shop) run_shop ;;
        ledger) run_ledger ;;
        saas) run_saas ;;
        analytics) run_analytics ;;
        python) run_python ;;
        go) run_go ;;
        dotnet) run_dotnet ;;
        rust) run_rust ;;
        swift) run_swift ;;
        kotlin-android) run_kotlin_android ;;
        flutter) run_flutter ;;
        *) echo "no example $e" >&2; exit 1 ;;
    esac
done
