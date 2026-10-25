#!/bin/sh
# site/install.sh against a release of its own making, served from localhost:
# nothing leaves the machine, and no real release has to exist.
#
#   sh site/test_install.sh
#
# What it holds: the installer puts both binaries where it was told and says
# how to reach them; it refuses a tarball whose checksum is not the release's,
# a platform with no build, and a version with no release, each installing
# nothing; and a copy cut short at any point runs nothing.

set -eu

here=$(cd "$(dirname "$0")" && pwd)
script=$here/install.sh
work=$(mktemp -d "${TMPDIR:-/tmp}/fenecdb-install-test.XXXXXX")
server_pid=
cleanup() {
    if [ -n "$server_pid" ]; then
        kill "$server_pid" 2>/dev/null || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

failed=0
ok() { printf 'ok    %s\n' "$1"; }
bad() {
    printf 'FAIL  %s\n' "$1" >&2
    failed=1
}

# expect NAME CONDITION...: the condition is a command; true is a pass.
expect() {
    name=$1
    shift
    if "$@"; then ok "$name"; else bad "$name"; fi
}
contains() { grep -F -q -- "$2" "$1"; }
lacks() { ! grep -F -q -- "$2" "$1"; }
absent() { [ ! -e "$1" ]; }
runs() { [ -x "$1" ] && "$1" --help 2>&1 | head -n 1 | grep -F -q "fenec-server $2"; }

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    else
        shasum -a 256 "$1" | awk '{ print $1 }'
    fi
}

# The release: v9.9.9, a tarball for each platform holding stand-ins for the
# two binaries (the server's first line of --help is its version, as the real
# one's is), and the SHA256SUMS release.yml writes.
version=9.9.9
rel=$work/site/download/v$version
mkdir -p "$rel" "$work/site/latest/download" "$work/stage"
for target in aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
    d=$work/stage/fenec-v$version-$target
    mkdir -p "$d"
    printf '#!/bin/sh\necho "fenec shell"\n' >"$d/fenec"
    printf '#!/bin/sh\necho "fenec-server %s" >&2\n' "$version" >"$d/fenec-server"
    chmod 755 "$d/fenec" "$d/fenec-server"
    echo "licence" >"$d/LICENSE"
    tar -czf "$rel/fenec-v$version-$target.tar.gz" -C "$work/stage" "fenec-v$version-$target"
done
: >"$rel/SHA256SUMS"
for f in "$rel"/*.tar.gz; do
    printf '%s  %s\n' "$(sha256 "$f")" "$(basename "$f")" >>"$rel/SHA256SUMS"
done
cp "$rel/SHA256SUMS" "$work/site/latest/download/SHA256SUMS"

# A server on a port the system picks, found by the file it writes.
python3 - "$work/site" "$work/port" <<'EOF' &
import functools, http.server, sys


class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass


server = http.server.ThreadingHTTPServer(
    ("127.0.0.1", 0), functools.partial(Quiet, directory=sys.argv[1])
)
open(sys.argv[2], "w").write(str(server.server_address[1]))
server.serve_forever()
EOF
server_pid=$!
i=0
while [ ! -s "$work/port" ]; do
    i=$((i + 1))
    [ "$i" -lt 100 ] || { echo "the test server did not start" >&2; exit 1; }
    sleep 0.1
done
base=http://127.0.0.1:$(cat "$work/port")

# install NAME [VAR=value...]: a run into a directory of its own, its output
# in $work/NAME.out and its status in $work/NAME.rc.
install() {
    name=$1
    shift
    mkdir -p "$work/$name-home"
    set +e
    env HOME="$work/$name-home" FENEC_BASE_URL="$base" FENEC_INSTALL="$work/$name-inst" "$@" \
        sh "$script" >"$work/$name.out" 2>&1
    echo $? >"$work/$name.rc"
    set -e
}
status() { [ "$(cat "$work/$1.rc")" = "$2" ]; }
failing() { [ "$(cat "$work/$1.rc")" != 0 ]; }

# The latest release, installed.
install latest PATH="$PATH"
expect "latest: exit 0" status latest 0
expect "latest: found $version" contains "$work/latest.out" "Installed fenecdb $version"
expect "latest: checked the sum" contains "$work/latest.out" "Checksum ok"
expect "latest: fenec-server runs" runs "$work/latest-inst/bin/fenec-server" "$version"
expect "latest: fenec is there" test -x "$work/latest-inst/bin/fenec"
expect "latest: no temp files left in bin" test "$(find "$work/latest-inst/bin" -type f | wc -l | tr -d ' ')" -eq 2
expect "latest: says how to reach it" contains "$work/latest.out" 'export PATH="'
expect "latest: touched no startup file" test -z "$(ls -A "$work/latest-home")"

# Run again over it, as an upgrade.
install again PATH="$PATH"
expect "again: exit 0" status again 0
install again PATH="$PATH"
expect "again, over the first: exit 0" status again 0
expect "again: still two files" test "$(find "$work/again-inst/bin" -type f | wc -l | tr -d ' ')" -eq 2

# Already on the PATH: nothing to say about it.
install onpath PATH="$work/onpath-inst/bin:$PATH"
expect "on PATH: exit 0" status onpath 0
expect "on PATH: no hint" lacks "$work/onpath.out" "is not on your PATH"

# A version asked for, with and without the v.
install pinned FENEC_VERSION=v$version
expect "pinned v: exit 0" status pinned 0
install pinned2 FENEC_VERSION=$version
expect "pinned: exit 0" status pinned2 0
install nosuch FENEC_VERSION=1.2.3
expect "no such release: fails" failing nosuch
expect "no such release: says so" contains "$work/nosuch.out" "is there a release v1.2.3"
expect "no such release: installs nothing" absent "$work/nosuch-inst"
install oddversion 'FENEC_VERSION=1.0/../x'
expect "odd version: fails" failing oddversion
expect "odd version: installs nothing" absent "$work/oddversion-inst"

# A tarball that is not the one the sums name: every platform's, so the test
# does not have to work out which is this machine's, with a byte added.
mkdir -p "$work/good"
cp "$rel"/*.tar.gz "$work/good/"
for f in "$rel"/*.tar.gz; do
    printf 'x' >>"$f"
done
install tampered PATH="$PATH"
expect "tampered: fails" failing tampered
expect "tampered: names the checksum" contains "$work/tampered.out" "checksum"
expect "tampered: installs nothing" absent "$work/tampered-inst"
cp "$work/good"/*.tar.gz "$rel/"

# A platform with no build, and one that is not a platform: a uname that
# answers otherwise, ahead of the real one on PATH.
mkdir -p "$work/shim"
cat >"$work/shim/uname" <<'EOF'
#!/bin/sh
case "$1" in
    -s) echo "$FAKE_OS" ;;
    -m) echo "$FAKE_CPU" ;;
    *) echo "$FAKE_OS" ;;
esac
EOF
chmod 755 "$work/shim/uname"
install windows PATH="$work/shim:$PATH" FAKE_OS=MINGW64_NT-10.0 FAKE_CPU=x86_64
expect "windows: fails" failing windows
expect "windows: points to Docker or WSL" contains "$work/windows.out" "WSL"
expect "windows: installs nothing" absent "$work/windows-inst"
install freebsd PATH="$work/shim:$PATH" FAKE_OS=FreeBSD FAKE_CPU=amd64
expect "freebsd: fails" failing freebsd
expect "freebsd: names the OS" contains "$work/freebsd.out" "FreeBSD"
install arm32 PATH="$work/shim:$PATH" FAKE_OS=Linux FAKE_CPU=armv7l
expect "armv7: fails" failing arm32
expect "armv7: names the processor" contains "$work/arm32.out" "armv7l"
expect "armv7: installs nothing" absent "$work/arm32-inst"
install linuxarm PATH="$work/shim:$PATH" FAKE_OS=Linux FAKE_CPU=aarch64
expect "linux aarch64: finds its build" contains "$work/linuxarm.out" "aarch64-unknown-linux-musl"

# A base URL that is not https (or localhost).
install plain FENEC_BASE_URL=http://example.com/releases
expect "http mirror: refused" failing plain
expect "http mirror: installs nothing" absent "$work/plain-inst"

# An argument it does not know is refused, and --help installs nothing.
set +e
sh "$script" --nonsense >"$work/arg.out" 2>&1
arg_rc=$?
sh "$script" --help >"$work/help.out" 2>&1
help_rc=$?
set -e
expect "unknown argument: refused" test "$arg_rc" -ne 0
expect "--help: exit 0" test "$help_rc" -eq 0
expect "--help: usage" contains "$work/help.out" "FENEC_VERSION"

# The script cut short at any point runs nothing: every line's end, and bytes
# across the file and in the last line.
size=$(wc -c <"$script" | tr -d ' ')
last=$(( size - $(tail -n 1 "$script" | wc -c | tr -d ' ') ))
cuts=$(awk 'BEGIN { n = 0 } { n += length($0) + 1; print n }' "$script" | sed '$d')
step=$((size / 60 + 1))
n=0
while [ "$n" -le "$((last + 3))" ]; do
    cuts="$cuts $n"
    n=$((n + step))
done
cuts="$cuts $last $((last + 1)) $((last + 2)) $((last + 3))"
count=0
broken=0
for cut in $cuts; do
    [ "$cut" -le "$((last + 3))" ] || continue
    rm -rf "$work/cut-inst"
    if [ "$cut" -eq 0 ]; then : >"$work/cut.sh"; else head -c "$cut" "$script" >"$work/cut.sh"; fi
    set +e
    env HOME="$work/latest-home" FENEC_BASE_URL="$base" FENEC_INSTALL="$work/cut-inst" sh <"$work/cut.sh" >"$work/cut.out" 2>&1
    set -e
    count=$((count + 1))
    if [ -e "$work/cut-inst" ] || contains "$work/cut.out" "Looking for"; then
        broken=$((broken + 1))
        echo "cut at byte $cut ran something" >&2
    fi
done
expect "cut short ($count cuts): nothing runs" test "$broken" -eq 0

if [ "$failed" -ne 0 ]; then
    echo "install.sh: FAILED" >&2
    exit 1
fi
echo "install.sh: all passed"
