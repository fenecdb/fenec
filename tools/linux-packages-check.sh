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
# before it is attached to it, and every assertion that fails says what it
# found and what it wanted. A pre-release's package carries `~` where the
# version has `-` (0.2.0-rc1 is 0.2.0~rc1, which sorts before 0.2.0), and the
# check holds it to that.
#
# FENEC_BINARY_VERSION is the version the binaries say they are, when that is
# not VERSION: a test that names a release's tarballs for another version.
#
# The binaries of 0.1.13 and later are asked `fenec --version` and
# `fenec-server --version`, each held to `<name> <version>` on stdout; those
# of a release before it, which have no such flag, name their version on the
# first line of `fenec-server --help` alone. Which is decided by the
# binaries' version, not the package's: CI checks the latest release, which
# may be one from before the flag. tools/homebrew-formula.py cuts over at the
# same release (VERSION_FLAG).

set -eu

# at_least VERSION A.B.C: whether VERSION, what follows a `-`, `~` or `+` in
# it left out (0.2.0-rc1, a package's 0.2.0~rc1, a build's +meta), is A.B.C
# or later, compared number by number (0.1.9 is before 0.1.13). Either one
# not three numbers is refused, rather than read: `[` taking `12~rc1` for a
# number errs, and the comparison went on past it to a yes.
at_least() {
    have=${1%%[-~+]*}
    want=${2%%[-~+]*}
    for v in "$have" "$want"; do
        case $v in
            *[!0-9.]* | .* | *. | *..* | *.*.*.*) ;;
            *.*.*) continue ;;
        esac
        echo "not a version of three numbers: [$v], from [$1] or [$2]" >&2
        exit 2
    done
    for _ in 1 2 3; do
        h=${have%%.*}
        w=${want%%.*}
        [ "$h" -gt "$w" ] && return 0
        [ "$h" -lt "$w" ] && return 1
        have=${have#*.}
        want=${want#*.}
    done
    return 0
}

[ $# -eq 3 ] || { sed -n '2,/^$/s/^# \{0,1\}//p' "$0" >&2; exit 2; }
version=${1#v}
dir=$(cd "$2" && pwd)
arch=$3
case $arch in
    amd64) rpmarch=x86_64 ;;
    arm64) rpmarch=aarch64 ;;
    *) echo "no such architecture: $arch (amd64 or arm64)" >&2; exit 2 ;;
esac
# nfpm's semver schema: the first `-` of the version becomes `~`.
pkgversion=$(printf '%s\n' "$version" | sed 's/-/~/')
base=${version%%-*}
deb=fenecdb_${version}-1_$arch.deb
rpm=fenecdb-$version-1.$rpmarch.rpm
binary_version=${FENEC_BINARY_VERSION:-$version}
if at_least "$binary_version" 0.1.13; then version_flag=yes; else version_flag=no; fi

# What each container's shell knows: assertions that print what they found,
# and the run both packages must pass once installed. Each binary names its
# version on stdout under --version, or, before 0.1.13, fenec-server on the
# first line of --help; the shell is run on a script, in memory.
lib=$(
    cat <<'EOF'
set -eu
fails() { echo "FAIL: $*" >&2; exit 1; }
eq() { [ "$2" = "$3" ] || fails "$1: found [$2], wanted [$3]"; }
isexec() { [ -x "$2" ] || fails "$1: $2 is not an executable file"; }
isfile() { [ -f "$2" ] || fails "$1: $2 is not a file"; }
gone() { [ ! -e "$2" ] || fails "$1: $2 is still there"; }
run_it() {
    isexec "installed fenec" /usr/bin/fenec
    isexec "installed fenec-server" /usr/bin/fenec-server
    if [ "$VERSION_FLAG" = yes ]; then
        # stdout alone: what a script reads from it.
        for b in fenec fenec-server; do
            first=$("/usr/bin/$b" --version) || fails "$b --version exited with an error, printing [$first]"
            eq "$b --version" "$first" "$b ${BINARY_VERSION}"
        done
    else
        first=$(/usr/bin/fenec-server --help 2>&1 | head -n 1) || true
        eq "fenec-server --help, first line" "$first" "fenec-server ${BINARY_VERSION}"
    fi
    out=$(printf 'create collection t (n int)\nput t {id: 1, n: 7}\nget t\n' | /usr/bin/fenec 2>&1) ||
        fails "fenec exited with an error, saying [$out]"
    printf '%s\n' "$out" | grep -q '^1 *7$' || fails "fenec answered [$out], wanted a row 1 7"
    echo "ran: $first"
}
EOF
)

deb_checks=$(
    cat <<'EOF'
f=/pkg/$FILE
eq "deb Package" "$(dpkg-deb -f "$f" Package)" fenecdb
eq "deb Version" "$(dpkg-deb -f "$f" Version)" "$PKGVERSION-1"
eq "deb Architecture" "$(dpkg-deb -f "$f" Architecture)" "$ARCH"
eq "deb Homepage" "$(dpkg-deb -f "$f" Homepage)" https://fenecdb.com
eq "deb Depends" "$(dpkg-deb -f "$f" Depends)" ""
dpkg -i "$f"
eq "installed version" "$(dpkg-query -W -f='${Version}' fenecdb)" "$PKGVERSION-1"
if [ "$VERSION" != "$BASE" ]; then
    dpkg --compare-versions "$PKGVERSION-1" lt "$BASE-1" ||
        fails "the pre-release $PKGVERSION-1 does not sort before $BASE-1"
fi
isfile "deb licence" /usr/share/doc/fenecdb/copyright
run_it
dpkg -r fenecdb
gone "after dpkg -r" /usr/bin/fenec
gone "after dpkg -r" /usr/bin/fenec-server
echo removed
EOF
)

rpm_checks=$(
    cat <<'EOF'
f=/pkg/$FILE
q() { rpm -qp --queryformat "$1" "$f"; }
eq "rpm Name" "$(q '%{NAME}')" fenecdb
eq "rpm Version" "$(q '%{VERSION}')" "$PKGVERSION"
eq "rpm Release" "$(q '%{RELEASE}')" 1
eq "rpm Arch" "$(q '%{ARCH}')" "$RPMARCH"
eq "rpm License" "$(q '%{LICENSE}')" Apache-2.0
eq "rpm URL" "$(q '%{URL}')" https://fenecdb.com
eq "rpm requirements besides rpmlib" "$(rpm -qp --requires "$f" | grep -v '^rpmlib(' || true)" ""
if [ "$VERSION" != "$BASE" ]; then
    eq "rpm: the pre-release sorts before its release" \
        "$(rpm --eval "%{lua: print(rpm.vercmp('$PKGVERSION', '$BASE'))}")" -1
fi
dnf -q install -y "$f"
eq "installed version" "$(rpm -q --queryformat '%{VERSION}-%{RELEASE}' fenecdb)" "$PKGVERSION-1"
isfile "rpm licence" /usr/share/licenses/fenecdb/LICENSE
run_it
dnf -q remove -y fenecdb
gone "after dnf remove" /usr/bin/fenec
gone "after dnf remove" /usr/bin/fenec-server
echo removed
EOF
)

# in_container IMAGE FILE CHECKS
in_container() {
    echo "== $2 in $1 ($arch)"
    docker run --rm --platform "linux/$arch" -v "$dir:/pkg:ro" \
        -e "FILE=$2" -e "VERSION=$version" -e "BASE=$base" -e "PKGVERSION=$pkgversion" \
        -e "BINARY_VERSION=$binary_version" -e "VERSION_FLAG=$version_flag" \
        -e "ARCH=$arch" -e "RPMARCH=$rpmarch" \
        "$1" /bin/sh -c "$lib
$3"
}

in_container debian:12-slim "$deb" "$deb_checks"
in_container fedora:42 "$rpm" "$rpm_checks"
echo "both installed, ran and removed"
