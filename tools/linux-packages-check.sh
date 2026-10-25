#!/bin/sh
# Install the release's .deb in a Debian container and its .rpm in a Fedora
# one, run what they installed, and remove it:
#
#   tools/linux-packages-check.sh VERSION DIR ARCH      ARCH: amd64 or arm64
#
# DIR holds what tools/linux-packages.sh wrote. The containers are of ARCH
# (--platform), so on a machine of the other architecture Docker emulates
# one; release.yml runs each architecture on a runner of its own. A package
# that does not install, or whose binaries do not run, fails the release
# before it is attached to it.

set -eu

[ $# -eq 3 ] || { sed -n '2,/^$/s/^# \{0,1\}//p' "$0" >&2; exit 2; }
version=${1#v}
dir=$(cd "$2" && pwd)
arch=$3
case $arch in
    amd64) rpmarch=x86_64 ;;
    arm64) rpmarch=aarch64 ;;
    *) echo "no such architecture: $arch (amd64 or arm64)" >&2; exit 2 ;;
esac

# What both must hold once installed. `fenec-server --help` names the
# version on its first line; the shell is run on a script, in memory.
check() {
    cat <<EOF
set -eu
test -x /usr/bin/fenec
test -x /usr/bin/fenec-server
first=\$(/usr/bin/fenec-server --help 2>&1 | head -n 1)
[ "\$first" = "fenec-server $version" ] || { echo "fenec-server says '\$first'" >&2; exit 1; }
out=\$(printf 'create collection t (n int)\nput t {id: 1, n: 7}\nget t\n' | /usr/bin/fenec)
printf '%s\n' "\$out" | grep -q '^1 *7\$' || { echo "fenec answered: \$out" >&2; exit 1; }
echo "ran: \$first"
EOF
}

deb=fenecdb_${version}-1_$arch.deb
echo "== $deb in debian:12-slim ($arch)"
docker run --rm --platform "linux/$arch" -v "$dir:/pkg:ro" debian:12-slim /bin/sh -c "
set -eu
dpkg -i /pkg/$deb
[ \"\$(dpkg-deb -f /pkg/$deb Package)\" = fenecdb ]
[ \"\$(dpkg-deb -f /pkg/$deb Architecture)\" = $arch ]
[ \"\$(dpkg-deb -f /pkg/$deb Version)\" = $version-1 ]
[ \"\$(dpkg-deb -f /pkg/$deb Homepage)\" = https://fenecdb.com ]
dpkg-deb -f /pkg/$deb Depends | grep . && { echo 'the package depends on something'; exit 1; } || true
test -f /usr/share/doc/fenecdb/copyright
$(check)
dpkg -r fenecdb
test ! -e /usr/bin/fenec
test ! -e /usr/bin/fenec-server
echo 'removed'
"

rpm=fenecdb-$version-1.$rpmarch.rpm
echo "== $rpm in fedora:42 ($arch)"
docker run --rm --platform "linux/$arch" -v "$dir:/pkg:ro" fedora:42 /bin/sh -c "
set -eu
rpm -qp --queryformat '%{NAME} %{VERSION} %{RELEASE} %{ARCH} %{LICENSE} %{URL}\n' /pkg/$rpm
[ \"\$(rpm -qp --queryformat '%{NAME}' /pkg/$rpm)\" = fenecdb ]
[ \"\$(rpm -qp --queryformat '%{ARCH}' /pkg/$rpm)\" = $rpmarch ]
[ \"\$(rpm -qp --queryformat '%{LICENSE}' /pkg/$rpm)\" = Apache-2.0 ]
[ \"\$(rpm -qp --queryformat '%{URL}' /pkg/$rpm)\" = https://fenecdb.com ]
rpm -qp --requires /pkg/$rpm | grep -v '^rpmlib(' && { echo 'the package requires something'; exit 1; } || true
dnf -q install -y /pkg/$rpm
$(check)
test -f /usr/share/licenses/fenecdb/LICENSE
dnf -q remove -y fenecdb
test ! -e /usr/bin/fenec
test ! -e /usr/bin/fenec-server
echo 'removed'
"
echo "both installed, ran and removed"
