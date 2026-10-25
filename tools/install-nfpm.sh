#!/bin/sh
# nfpm, which makes the release's .deb and .rpm (tools/linux-packages.sh),
# into DIR:
#
#   tools/install-nfpm.sh DIR
#
# The release binary named here, held to the checksum copied from that
# release's checksums.txt (https://github.com/goreleaser/nfpm/releases): a
# download that is not the file that was looked at is refused. A newer nfpm
# is a change of the version and of all four sums below, made on purpose.
# release.yml and ci.yml both come through here, so they cannot disagree.

set -eu

version=2.48.0

[ $# -eq 1 ] || { sed -n '2,/^$/s/^# \{0,1\}//p' "$0" >&2; exit 2; }
dir=$1

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)
        file=nfpm_${version}_Linux_x86_64.tar.gz
        sum=6d7f9a28b2f397934403950fcbba83f598b08d02edbe79a0815b3b4897341750
        ;;
    Linux-aarch64 | Linux-arm64)
        file=nfpm_${version}_Linux_arm64.tar.gz
        sum=a9a75213763783b7669719e505474c748c5ad7dc66591a195619deb50f85c2f4
        ;;
    Darwin-arm64)
        file=nfpm_${version}_Darwin_arm64.tar.gz
        sum=ae23377895437b18d4727b362e7ec4c2d38295eb9adf96008e0cbc813038df81
        ;;
    Darwin-x86_64)
        file=nfpm_${version}_Darwin_x86_64.tar.gz
        sum=be3bdff94bae08638da0f7a3cc4b4ea3f381fa3f10560fcf81ec2e8fea625b49
        ;;
    *)
        echo "no nfpm build pinned for $(uname -s) $(uname -m)" >&2
        exit 1
        ;;
esac

tmp=$(mktemp -d "${TMPDIR:-/tmp}/install-nfpm.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
curl -fsSL --retry 3 -o "$tmp/$file" "https://github.com/goreleaser/nfpm/releases/download/v$version/$file"

if command -v sha256sum >/dev/null 2>&1; then
    got=$(sha256sum "$tmp/$file" | awk '{ print $1 }')
else
    got=$(shasum -a 256 "$tmp/$file" | awk '{ print $1 }')
fi
if [ "$got" != "$sum" ]; then
    echo "$file is $got, not the $sum this script holds it to" >&2
    exit 1
fi

mkdir -p "$dir"
tar -xzf "$tmp/$file" -C "$dir" nfpm
echo "nfpm $version in $dir"
