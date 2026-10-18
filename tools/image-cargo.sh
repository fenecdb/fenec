#!/bin/sh
# `cargo build` inside an image's build: binaries made from the sources the
# build was given, and held to them before they are copied out.
#
#   RUN --mount=type=cache,id=fenec-image-cargo,target=/work,sharing=locked \
#       tools/image-cargo.sh /built --release -p fenec-server
#
# Run from the build context's copy (the Dockerfiles' /src) with /work a
# cache mount that only this script uses. Every binary the build makes is
# copied into the first argument; the rest are `cargo build`'s.
#
# Why /work keeps a copy of the sources: cargo takes a file for unchanged
# when its mtime is no later than the output built from it, and COPY keeps
# the mtimes the build context had. With target/ in a cache mount every
# build shares, a build from files older on disk than the last build -- a
# worktree made earlier, a branch whose files kept their times -- found
# every output newer than every source and compiled nothing: a branch's
# image carried main's fenec-shard, byte for byte. So the sources are
# copied into /work/src by content (rsync -c, no times kept): a file whose
# bytes changed is written again and takes the time it is written at, every
# other keeps the one it had, and /work/src is a tree edited in place, the
# case cargo's freshness is right about. Rejected: touching every file
# copied, which compiles the whole workspace every build -- none of these
# crates has a dependency of its own for the cache to keep -- and cargo's
# -Zchecksum-freshness, which needs a nightly cargo and rustc.
#
# That also makes the binaries hang on the sources alone, which BuildKit
# takes for granted: a RUN's cache key leaves its cache mounts out, and a
# COPY --from of its output is keyed by that output's content. When stale
# binaries came out of a warm target/ and fresh ones out of an empty, one
# key held two outputs: rebuilt with --no-cache, BuildKit filed the second
# build's COPY layer under the first's content, and a later build making
# the first's binaries again was handed the second's layer -- main's build
# got a branch's fenec-shard though its own build stage held main's. The
# Dockerfiles copy from a path no build before this script wrote, so no
# key filed that way is reached.
#
# And a check that trusts none of that: cargo runs every compile through
# this script (RUSTC_WRAPPER), which, once rustc has succeeded, records the
# sha256 of each file the compile read -- the unit's dep-info, its sources
# and what it include_bytes! alike, and the libraries it was handed
# (--extern), each as cargo named it: a library's .rmeta to a library, its
# .rlib to a binary. Held to its sources alone, a binary whose own outputs
# looked newer than a library compiled since for another binary was taken
# for fresh, every record matching, and linked the library as it had been.
# Not each library's record instead, which is smaller to hash: cargo starts
# a library's dependents on its .rmeta, while its own compile -- and the
# record written after it -- is still running. Cargo counts a unit built
# only once the wrapper has exited 0, so every unit it takes for fresh has
# the record of the compile that made it. After the build every unit of
# the binaries, fresh or compiled now, is held to its record against the
# build context itself: a binary made from other bytes than this build's
# fails the build, naming the files, and the outputs kept in /work are
# emptied so that the next build starts clean.
#
# Not followed yet: what a build script reads when it runs, which cargo
# tracks by mtime alone (its rerun-if-changed), and files a proc-macro opens
# itself. No crate here has a build script, and one is refused below until
# the check holds its runs too; a proc-macro's own compile is checked as any.
set -eu

# ---------------------------------------------------------------- wrapper
# Cargo calls `$RUSTC_WRAPPER <rustc> <arguments>`.
if [ -n "${FENEC_IMAGE_RECORDS:-}" ]; then
    "$@" || exit
    shift
    out='' name='' extra='' prev='' externs=''
    for a do
        case $prev in
        --out-dir) out=$a ;;
        --crate-name) name=$a ;;
        -C) case $a in extra-filename=*) extra=${a#extra-filename=} ;; esac ;;
        # name=path, priv:name=path; a bare name is the sysroot's.
        --extern) case $a in *=*) externs="$externs ${a#*=}" ;; esac ;;
        esac
        prev=$a
    done
    # rustc asked what it is (-vV, --print): no unit.
    if [ -z "$out" ] || [ -z "$name" ]; then exit 0; fi
    d=$out/$name$extra.d
    # The dep-info's rule for itself names every file the compile read.
    deps=$(awk -v t="$d:" 'index($0, t) == 1 { print substr($0, length(t) + 1); exit }' "$d")
    case $deps in
    '' | *'\ '*)
        echo "image-cargo: cannot read the files $d names" >&2
        exit 1
        ;;
    esac
    record=$FENEC_IMAGE_RECORDS/$name$extra
    set -f
    # shellcheck disable=SC2086 # one word a file
    sha256sum $deps $externs > "$record.$$"
    mv -f "$record.$$" "$record"
    exit 0
fi

# ------------------------------------------------------------------ build
if [ $# -lt 1 ]; then
    echo "usage: image-cargo.sh <out dir> <cargo build arguments>" >&2
    exit 2
fi
out=$1
shift
context=$PWD
self=$(cd "$(dirname "$0")" && pwd)/${0##*/}
work=/work
tree=$work/src
target=$work/target
records=$work/records
units=$work/units.json
mkdir -p "$tree" "$target" "$records" "$out"

rsync -rlc --delete "$context/" "$tree/"

cd "$tree"
CARGO_TARGET_DIR=$target RUSTC_WRAPPER=$self FENEC_IMAGE_RECORDS=$records \
    cargo build --message-format=json-render-diagnostics "$@" > "$units"

# ------------------------------------------------------------------ check
cd "$context"
fail() {
    printf 'image-cargo: %s\n' "$@" >&2
    rm -rf "$target" "$records"
    echo "image-cargo: the build's outputs in $work were emptied; the next build starts clean" >&2
    exit 1
}
checked=0
bins=''
# One line a unit, fresh or compiled now: every one the binaries are made of.
while IFS= read -r m; do
    case $m in *'"reason":"compiler-artifact"'*) ;; *) continue ;; esac
    case $m in
    *'"kind":["custom-build"]'*)
        printf 'image-cargo: %s has a build script, which the check does not follow yet:\n' \
            "$(printf '%s\n' "$m" | sed -n 's/.*"package_id":"\([^"]*\)".*/\1/p')" >&2
        echo "image-cargo: what it reads when it runs, cargo tracks by mtime alone. Have" \
            "tools/image-cargo.sh hold each run's inputs (cargo's build-script-executed" \
            "messages name its out_dir, whose output lists them) before adding one." >&2
        exit 1
        ;;
    esac
    exe=$(printf '%s\n' "$m" | sed -n 's/.*"executable":"\([^"]*\)".*/\1/p')
    if [ -n "$exe" ]; then
        # A binary is a hard link out of deps/ to the unit that made it.
        unit=$(find "${exe%/*}/deps" -maxdepth 1 -type f -inum "$(stat -c %i "$exe")" | head -n 1)
        unit=${unit##*/}
        bins="$bins $exe"
    else
        # A library names its metadata in deps/, lib<unit>.rmeta; a
        # proc-macro, which has none, its shared object.
        unit=$(printf '%s\n' "$m" | grep -oE '/deps/lib[^"/]*\.(rmeta|so|dylib)"' | head -n 1)
        unit=${unit#/deps/lib}
        unit=${unit%\"}
        unit=${unit%.*}
    fi
    [ -n "$unit" ] || fail "cannot tell which unit this is: $m"
    record=$records/$unit
    [ -f "$record" ] || fail "$unit: no record of the files it was compiled from"
    # The record's paths are the tree's; asked of the build context's copy.
    if ! said=$(sed "s|  $tree/|  |" "$record" | sha256sum -c - 2>&1); then
        fail "$unit was compiled from other bytes than this build's:" \
            "$(printf '%s\n' "$said" | grep -v ': OK$')"
    fi
    checked=$((checked + 1))
done < "$units"
[ -n "$bins" ] || fail "cargo built no binary"

# shellcheck disable=SC2086 # one word a binary
cp $bins "$out/"
echo "image-cargo: $checked units held to the files they were compiled from:$bins" >&2
