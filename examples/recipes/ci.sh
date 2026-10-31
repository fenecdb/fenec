#!/bin/sh
# The recipes, held to what the docs paste (`make recipes-test`): auth.sh, the
# files demo against an S3-compatible store in Docker and the functions demo,
# each against this checkout's fenec-server and -- through
# examples/run-tests.sh -- this checkout's @fenecdb/web, and their outputs held to
# golden/*.txt, which the pages of the docs are held to in turn. Small: one
# server at a time, no clients in numbers.
#
#   examples/run-tests.sh recipes          packs web/, installs, runs this
#   RECIPES_UPDATE=1 ... recipes           writes golden/*.txt again
#
#   FENEC_SERVER   the server binary          (default target/debug/fenec-server)
#   RECIPES_GOLDEN where golden/ is            (default beside this script; run-tests.sh points it
#                  into the checkout, so that an update is written there and not into a copy)
#   RECIPES_PAGES  site/content/docs          (the pages the golden is held to; none: not checked)
#   WRANGLER       a wrangler to run the Worker with; under CI one is installed,
#                  pinned, where there is none, and without one the Worker's
#                  section is left out
#
# Under CI a tool that is missing fails the run. Elsewhere its recipe is skipped.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
export FENEC_SERVER=${FENEC_SERVER:-$here/../../target/debug/fenec-server}
golden=${RECIPES_GOLDEN:-$here/golden}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

failed=0
skipped=""

missing() { # missing <why> <what needs it>
    echo "$1 -- $2 skipped"
    [ -z "${CI:-}" ] || failed=1
    skipped="$skipped $2"
}
has() { command -v "$1" >/dev/null 2>&1; }

# held <name> <output file> <pages...>: the output against golden/<name>.txt, and the golden against the pages.
held() {
    name=$1
    out=$2
    shift 2
    if [ -n "${RECIPES_UPDATE:-}" ]; then
        if grep -q '^(skipped' "$out"; then
            echo "FAIL $name: a section was skipped, and the golden would lose it"
            failed=1
            return
        fi
        python3 "$here/golden.py" normalize <"$out" >"$golden/$name.txt"
        echo "wrote golden/$name.txt"
        return
    fi
    if python3 "$here/golden.py" check "$golden/$name.txt" "$out"; then
        echo "ok   $name matches golden/$name.txt"
    else
        echo "FAIL $name"
        failed=1
        return
    fi
    if [ -n "${RECIPES_PAGES:-}" ]; then
        pages=""
        for p in "$@"; do pages="$pages $RECIPES_PAGES/$p.html"; done
        # shellcheck disable=SC2086
        if python3 "$here/golden.py" pages "$golden/$name.txt" $pages; then
            echo "ok   $name is on the pages"
        else
            echo "FAIL $name: a line of the run is on no page"
            failed=1
        fi
    fi
}

for tool in node curl jq python3; do
    has "$tool" || { missing "$tool not found" "the recipes"; exit "$failed"; }
done
[ -x "$FENEC_SERVER" ] || { echo "no $FENEC_SERVER: cargo build -p fenec-server"; exit 1; }
[ -d "$here/node_modules/@aws-sdk" ] || { echo "no node_modules: npm install (examples/run-tests.sh does)"; exit 1; }

echo "== auth"
sh "$here/auth.sh" >"$tmp/auth.out" 2>&1 || { cat "$tmp/auth.out"; exit 1; }
held auth "$tmp/auth.out" auth

echo "== files"
if ! has docker; then
    missing "docker not found" "the files recipe"
elif ! docker info >/dev/null 2>&1; then
    missing "docker is not usable (docker info fails)" "the files recipe"
else
    node "$here/files-demo.mjs" >"$tmp/files.out" 2>&1 || { cat "$tmp/files.out"; exit 1; }
    held files "$tmp/files.out" files
fi

echo "== functions"
if [ -z "${WRANGLER:-}" ] && ! has wrangler && [ -n "${CI:-}" ]; then
    # 4.149.0 is what integrations/cloudflare's lockfile resolves, which that job's
    # tests run the example Worker under in CI (its package.json says ^4.144.0, and
    # site.yml pins 4.135.0 for the deploy action, which only has to serve assets).
    npm install --prefix "$tmp/wrangler" --no-audit --no-fund --loglevel=error wrangler@4.149.0 >&2
    WRANGLER="$tmp/wrangler/node_modules/.bin/wrangler"
    export WRANGLER
fi
if [ -z "${WRANGLER:-}" ] && ! has wrangler; then
    echo "wrangler not found -- the Worker's section is left out"
fi
node "$here/functions-demo.mjs" >"$tmp/functions.out" 2>&1 || { cat "$tmp/functions.out"; exit 1; }
held functions "$tmp/functions.out" functions

[ "$failed" = 0 ] || { echo "recipes: failed${skipped:+ (skipped:$skipped)}"; exit 1; }
echo "recipes: ok${skipped:+ (skipped:$skipped)}"
