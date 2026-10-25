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
        /bin/sh "$script" >"$work/$name.out" 2>&1
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

# A base URL that is not https, or not this machine's: plain http elsewhere,
# a user that makes `localhost` the user and another host the host, a host
# that only starts like localhost, another scheme.
n=0
for url in \
    http://example.com/releases \
    http://localhost:x@example.invalid \
    http://localhost:80@example.invalid/releases \
    http://127.0.0.1@example.invalid \
    http://localhost.example.invalid/releases \
    http://127.0.0.1.example.invalid \
    http://localhostx \
    https://user:secret@example.invalid/releases \
    "https://example.invalid/a b" \
    ftp://example.invalid/releases \
    file:///etc; do
    n=$((n + 1))
    install "url$n" "FENEC_BASE_URL=$url"
    expect "base URL $url: refused" failing "url$n"
    expect "base URL $url: installs nothing" absent "$work/url$n-inst"
    expect "base URL $url: says why" contains "$work/url$n.out" FENEC_BASE_URL
done
# And these are this machine's, or https.
install localhost "FENEC_BASE_URL=http://localhost:${base##*:}"
expect "http://localhost:port: installs" status localhost 0
install localhostslash "FENEC_BASE_URL=http://127.0.0.1:${base##*:}/"
expect "http://127.0.0.1:port/: installs" status localhostslash 0

# The tools it downloads with. Without curl on PATH, from links to what the
# installer needs and a wget of each kind in front: BusyBox's without
# ssl_client, which checks no certificate; BusyBox's with it (Alpine's), which
# is used; one that says it did not check what it fetched; GNU's, which is
# held to https; and one that is neither.
mkdir -p "$work/nocurl"
for tool in uname tar gzip awk sed grep head mktemp sha256sum shasum cat rm mkdir cp chmod mv sysctl; do
    path=$(command -v "$tool" 2>/dev/null || true)
    case $path in /*) ln -sf "$path" "$work/nocurl/$tool" ;; esac
done
curl_bin=$(command -v curl || true)
if [ -n "$curl_bin" ]; then
    # A wget that fetches with curl, for the cases that should go through.
    wget_shim() { # DIR HELP VERSION [NOTE]
        mkdir -p "$1"
        cat >"$1/wget" <<EOF
#!/bin/sh
case "\$1" in
    --help) echo '$2'; exit 0 ;;
    --version) echo '$3'; exit 0 ;;
esac
echo "\$@" >>"$1/wget.args"
[ -z '${4:-}' ] || echo '${4:-}' >&2
# -q [flags] -O FILE URL
for a in "\$@"; do last=\$a; done
out=
prev=
for a in "\$@"; do
    [ "\$prev" = -O ] && out=\$a
    prev=\$a
done
exec "$curl_bin" -fsSL -o "\$out" "\$last"
EOF
        chmod 755 "$1/wget"
    }

    wget_shim "$work/bb-nossl" 'BusyBox v1.36.1 multi-call binary.' 'wget: unrecognized option: version'
    install bbnossl PATH="$work/bb-nossl:$work/nocurl"
    expect "BusyBox wget without ssl_client: refused" failing bbnossl
    expect "BusyBox wget without ssl_client: says to install curl" contains "$work/bbnossl.out" "install curl"
    expect "BusyBox wget without ssl_client: installs nothing" absent "$work/bbnossl-inst"
    expect "BusyBox wget without ssl_client: downloads nothing" absent "$work/bb-nossl/wget.args"

    wget_shim "$work/bb-ssl" 'BusyBox v1.36.1 multi-call binary.' 'wget: unrecognized option: version'
    printf '#!/bin/sh\nexit 0\n' >"$work/bb-ssl/ssl_client"
    chmod 755 "$work/bb-ssl/ssl_client"
    install bbssl PATH="$work/bb-ssl:$work/nocurl"
    expect "BusyBox wget with ssl_client: installs" status bbssl 0
    expect "BusyBox wget with ssl_client: fenec-server runs" runs "$work/bbssl-inst/bin/fenec-server" "$version"

    wget_shim "$work/bb-note" 'BusyBox v1.36.1 multi-call binary.' 'wget: unrecognized option: version' 'wget: note: TLS certificate validation not implemented'
    cp "$work/bb-ssl/ssl_client" "$work/bb-note/ssl_client"
    install bbnote PATH="$work/bb-note:$work/nocurl"
    expect "wget that checks no certificate: refused" failing bbnote
    expect "wget that checks no certificate: says so" contains "$work/bbnote.out" "does not check TLS certificates"
    expect "wget that checks no certificate: installs nothing" absent "$work/bbnote-inst/bin/fenec"

    wget_shim "$work/gnu" 'Usage: wget [OPTION]...' 'GNU Wget 1.21.3 built on linux-gnu.'
    install gnuwget PATH="$work/gnu:$work/nocurl"
    expect "GNU wget: installs" status gnuwget 0
    expect "GNU wget over http: no https flags" lacks "$work/gnu/wget.args" "--https-only"
    : >"$work/gnu/wget.args"
    install gnuhttps PATH="$work/gnu:$work/nocurl" FENEC_BASE_URL=https://example.invalid
    expect "GNU wget over https: --https-only" contains "$work/gnu/wget.args" "--https-only"
    expect "GNU wget over https: TLS 1.2" contains "$work/gnu/wget.args" "--secure-protocol=TLSv1_2"

    wget_shim "$work/other" 'wget (some other)' 'other wget 1.0'
    install otherwget PATH="$work/other:$work/nocurl"
    expect "an unknown wget: refused" failing otherwget
    expect "an unknown wget: installs nothing" absent "$work/otherwget-inst"
fi
install nodownloader PATH="$work/nocurl"
expect "no curl or wget: refused" failing nodownloader
expect "no curl or wget: says what it needs" contains "$work/nodownloader.out" "needs curl or wget"

# A relative FENEC_INSTALL is told back as an absolute directory.
mkdir -p "$work/relcwd"
rel_dir=$(cd "$work/relcwd" && pwd)
set +e
(cd "$work/relcwd" && env FENEC_BASE_URL="$base" FENEC_INSTALL=rel/dir /bin/sh "$script" >"$work/rel.out" 2>&1)
rel_rc=$?
set -e
expect "relative FENEC_INSTALL: installs" test "$rel_rc" -eq 0
expect "relative FENEC_INSTALL: tells the absolute directory" contains "$work/rel.out" "$rel_dir/rel/dir/bin"
expect "relative FENEC_INSTALL: where it said" test -x "$rel_dir/rel/dir/bin/fenec"

# A copy that fails leaves nothing of its own behind (a directory nobody can
# write to means nothing as root, which writes anywhere).
if [ "$(id -u)" != 0 ]; then
    mkdir -p "$work/ro-inst/bin"
    chmod 555 "$work/ro-inst/bin"
    install ro FENEC_INSTALL="$work/ro-inst"
    expect "unwritable bin: fails" failing ro
    expect "unwritable bin: leaves no staged file" test "$(find "$work/ro-inst/bin" -type f | wc -l | tr -d ' ')" -eq 0
    chmod 755 "$work/ro-inst/bin"
fi

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
