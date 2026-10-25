#!/bin/sh
# The .deb and .rpm of a release, made from its musl tarballs.
#
#   tools/linux-packages.sh VERSION TARBALL_DIR OUT_DIR [amd64] [arm64]
#
# TARBALL_DIR holds the release's fenec-vVERSION-<arch>-unknown-linux-musl.tar.gz
# (release.yml's `binaries` job); OUT_DIR gets, for each architecture named
# (both when none is):
#
#   fenecdb_VERSION-1_amd64.deb    fenecdb-VERSION-1.x86_64.rpm
#   fenecdb_VERSION-1_arm64.deb    fenecdb-VERSION-1.aarch64.rpm
#
# The packages are nfpm's (packaging/nfpm.yaml), which makes both formats
# from one description and needs no dpkg or rpmbuild, nor a machine of the
# architecture it packs. `nfpm` is on PATH, or NFPM names it; release.yml
# installs the release it pins, verified by its checksum.
#
# Setting SOURCE_DATE_EPOCH makes the files' times that moment, so the same
# tarballs give the same packages.

set -eu

[ $# -ge 3 ] || { sed -n '2,/^$/s/^# \{0,1\}//p' "$0" >&2; exit 2; }
version=${1#v}
tarballs=$2
out=$3
shift 3
[ $# -gt 0 ] || set -- amd64 arm64

here=$(cd "$(dirname "$0")/.." && pwd)
nfpm=${NFPM:-nfpm}
command -v "$nfpm" >/dev/null 2>&1 || { echo "nfpm is not on PATH (or set NFPM)" >&2; exit 1; }

work=$(mktemp -d "${TMPDIR:-/tmp}/fenecdb-packages.XXXXXX")
trap 'rm -rf "$work"' EXIT
# nfpm globs the sources, and macOS's TMPDIR ends in a slash: `//` matches nothing.
work=$(cd "$work" && pwd -P)
mkdir -p "$out"
out=$(cd "$out" && pwd)
tarballs=$(cd "$tarballs" && pwd)

for arch in "$@"; do
    case $arch in
        amd64) triple=x86_64 ;;
        arm64) triple=aarch64 ;;
        *) echo "no such architecture: $arch (amd64 or arm64)" >&2; exit 2 ;;
    esac
    name=fenec-v$version-$triple-unknown-linux-musl
    [ -f "$tarballs/$name.tar.gz" ] || { echo "$tarballs/$name.tar.gz is not there" >&2; exit 1; }
    tar -xzf "$tarballs/$name.tar.gz" -C "$work"

    # The packages' contents are the tarball's binaries, nothing rebuilt.
    for b in fenec fenec-server; do
        [ -f "$work/$name/$b" ] || { echo "$name.tar.gz holds no $b" >&2; exit 1; }
    done

    for format in deb rpm; do
        case $format in
            deb) file=fenecdb_${version}-1_$arch.deb ;;
            rpm) file=fenecdb-$version-1.$triple.rpm ;;
        esac
        (
            cd "$here"
            NFPM_ARCH=$arch NFPM_VERSION=$version NFPM_BIN="$work/$name" \
                "$nfpm" package --config packaging/nfpm.yaml --packager "$format" --target "$out/$file"
        )
        echo "$file  $(wc -c <"$out/$file" | tr -d ' ') bytes"
    done
done
