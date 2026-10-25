#!/bin/sh
# Install fenecdb: `fenec`, the shell, and `fenec-server`, from a release.
#
#   curl -fsSL https://fenecdb.com/install.sh | sh
#
# Read it first with `curl -fsSL https://fenecdb.com/install.sh | less`. It
#
#   - works out the platform: macOS or Linux, arm64 or x86_64;
#   - downloads the release's tarball and its SHA256SUMS from GitHub, over
#     https, and checks the tarball against the sum before it unpacks it;
#   - puts `fenec` and `fenec-server` in a bin/ directory of your own: no sudo,
#     and no shell startup file is touched. It says how to put the directory
#     on your PATH, and that is the only thing left to you.
#
# Settings, all optional:
#
#   FENEC_VERSION   X.Y.Z (or vX.Y.Z); the latest release when unset
#   FENEC_INSTALL   where it goes: $FENEC_INSTALL/bin; default ~/.fenecdb
#   FENEC_BASE_URL  where releases are fetched from, for a mirror or a test:
#                   https://github.com/fenecdb/fenecdb/releases by default,
#                   and it must be https (http only for localhost)
#
# The checksums come from the same release as the tarball: they catch a
# truncated or damaged download, not a release someone replaced.
#
# The whole script is one function, called on the last line: a download cut
# short stops before that line and runs nothing.

set -eu

say() {
    printf '%s\n' "$*"
}

err() {
    printf 'fenecdb install: %s\n' "$*" >&2
}

die() {
    err "$*"
    exit 1
}

usage() {
    cat <<'EOF'
Install fenecdb (fenec, the shell, and fenec-server) from a release.

    curl -fsSL https://fenecdb.com/install.sh | sh
    curl -fsSL https://fenecdb.com/install.sh | sh -s -- --help

  FENEC_VERSION   X.Y.Z; the latest release when unset
  FENEC_INSTALL   installs into $FENEC_INSTALL/bin; default ~/.fenecdb

macOS and Linux on arm64 and x86_64. On Windows, use the Docker image
(ghcr.io/fenecdb/fenec-server) or run this inside WSL.
EOF
}

# The platform as the release names it: <arch>-<vendor>-<os>.
detect_target() {
    os=$(uname -s)
    cpu=$(uname -m)
    case $cpu in
        arm64 | aarch64) arch=aarch64 ;;
        x86_64 | amd64) arch=x86_64 ;;
        *) die "no fenecdb release for the processor '$cpu' (arm64 and x86_64 only). The Docker image, ghcr.io/fenecdb/fenec-server, is another way." ;;
    esac
    case $os in
        Darwin)
            # A shell started under Rosetta says x86_64 on an Apple silicon
            # Mac: the native build is the one to install.
            if [ "$arch" = x86_64 ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ]; then
                arch=aarch64
            fi
            target="$arch-apple-darwin"
            ;;
        Linux)
            # The Linux builds are static (musl): glibc or musl, nothing to match.
            target="$arch-unknown-linux-musl"
            ;;
        MINGW* | MSYS* | CYGWIN* | Windows_NT)
            die "fenecdb does not run on Windows yet. Use the Docker image (docker run ghcr.io/fenecdb/fenec-server) or run this script inside WSL."
            ;;
        *) die "no fenecdb release for '$os' (macOS and Linux only). The Docker image, ghcr.io/fenecdb/fenec-server, runs anywhere Docker does." ;;
    esac
}

# fetch URL FILE
fetch() {
    case $downloader in
        curl)
            case $1 in
                https://*) curl -fsSL --retry 3 --proto '=https' --tlsv1.2 -o "$2" "$1" ;;
                *) curl -fsSL --retry 3 -o "$2" "$1" ;;
            esac
            ;;
        wget) wget -q -O "$2" "$1" ;;
    esac
}

sha256_of() {
    case $hasher in
        sha256sum) sha256sum "$1" | awk '{ print $1 }' ;;
        shasum) shasum -a 256 "$1" | awk '{ print $1 }' ;;
    esac
}

cleanup() {
    if [ -n "${tmp:-}" ]; then
        rm -rf "$tmp"
    fi
}

main() {
    case ${1:-} in
        -h | --help)
            usage
            return 0
            ;;
        "") ;;
        *)
            usage >&2
            die "unknown argument '$1'"
            ;;
    esac

    base=${FENEC_BASE_URL:-https://github.com/fenecdb/fenecdb/releases}
    base=${base%/}
    case $base in
        https://* | http://localhost[:/]* | http://127.0.0.1[:/]* | http://localhost | http://127.0.0.1) ;;
        *) die "FENEC_BASE_URL must be https (http only for localhost): $base" ;;
    esac

    if [ -n "${FENEC_INSTALL:-}" ]; then
        home=$FENEC_INSTALL
    elif [ -n "${HOME:-}" ]; then
        home=$HOME/.fenecdb
    else
        die "HOME is not set: say where to install with FENEC_INSTALL"
    fi
    bindir=${home%/}/bin

    # What it will need, before it downloads anything.
    if command -v curl >/dev/null 2>&1; then
        downloader=curl
    elif command -v wget >/dev/null 2>&1; then
        downloader=wget
    else
        die "needs curl or wget to download the release"
    fi
    if command -v sha256sum >/dev/null 2>&1; then
        hasher=sha256sum
    elif command -v shasum >/dev/null 2>&1; then
        hasher=shasum
    else
        die "needs sha256sum or shasum to check the download"
    fi
    for tool in tar awk sed mktemp uname; do
        command -v "$tool" >/dev/null 2>&1 || die "needs '$tool'"
    done

    detect_target

    tmp=$(mktemp -d "${TMPDIR:-/tmp}/fenecdb-install.XXXXXX")
    trap cleanup EXIT
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM

    # The version: the one asked for, else the latest release's, which the
    # release's own SHA256SUMS names in the tarballs it lists.
    sums=$tmp/SHA256SUMS
    if [ -n "${FENEC_VERSION:-}" ]; then
        version=${FENEC_VERSION#v}
        case $version in
            *[!0-9A-Za-z.+-]* | "") die "FENEC_VERSION '$FENEC_VERSION' is not a version like 0.1.11" ;;
        esac
        say "Looking for fenecdb $version"
        fetch "$base/download/v$version/SHA256SUMS" "$sums" ||
            die "could not download $base/download/v$version/SHA256SUMS: is there a release v$version?"
    else
        say "Looking for the latest fenecdb release"
        fetch "$base/latest/download/SHA256SUMS" "$sums" ||
            die "could not download $base/latest/download/SHA256SUMS"
        version=$(sed -n "s/^[0-9a-f]\{64\}  *\**fenec-v\([^ ]*\)-$target\.tar\.gz\$/\1/p" "$sums" | head -n 1)
        case $version in
            *[!0-9A-Za-z.+-]* | "") die "the latest release lists no build for $target" ;;
        esac
    fi

    name=fenec-v$version-$target.tar.gz
    want=$(awk -v n="$name" '{ f = $2; sub(/^\*/, "", f); if (f == n) { print $1; exit } }' "$sums")
    [ -n "$want" ] || die "release v$version lists no $name: it has no build for this platform"

    say "Downloading $name"
    fetch "$base/download/v$version/$name" "$tmp/$name" ||
        die "could not download $base/download/v$version/$name"

    got=$(sha256_of "$tmp/$name")
    if [ "$got" != "$want" ]; then
        die "the checksum of $name is $got, SHA256SUMS says $want: nothing was installed"
    fi
    say "Checksum ok ($want)"

    tar -xzf "$tmp/$name" -C "$tmp" || die "could not unpack $name"
    src=$tmp/fenec-v$version-$target
    for b in fenec fenec-server; do
        [ -f "$src/$b" ] || die "$name holds no $b"
    done

    mkdir -p "$bindir" || die "could not make $bindir: say where to install with FENEC_INSTALL"
    for b in fenec fenec-server; do
        # Written beside and renamed over: a running server's file is not
        # overwritten in place ("Text file busy"), and a failure part of the
        # way leaves the old one whole.
        cp "$src/$b" "$bindir/.$b.new.$$" || die "could not write to $bindir"
        chmod 755 "$bindir/.$b.new.$$"
        mv -f "$bindir/.$b.new.$$" "$bindir/$b"
    done

    say ""
    say "Installed fenecdb $version ($target) in $bindir:"
    say "  fenec          the shell:  fenec notes.fenec"
    say "  fenec-server   the server: FENEC_HTTP_TOKEN=secret fenec-server --file data.fenec"

    # Does it start? Its version is the first line of --help.
    if line=$("$bindir/fenec-server" --help 2>&1 | head -n 1) && [ -n "$line" ]; then
        say "  ($line)"
    else
        err "installed, but $bindir/fenec-server did not start on this machine"
    fi

    case ":${PATH:-}:" in
        *":$bindir:"*) ;;
        *)
            shown=$bindir
            case $bindir in
                "${HOME:-/nonexistent}"/*) shown="\$HOME/${bindir#"$HOME"/}" ;;
            esac
            say ""
            say "$bindir is not on your PATH. Add it for this shell:"
            say ""
            say "  export PATH=\"$shown:\$PATH\""
            say ""
            say "and put the same line in your shell's startup file (~/.zshrc, ~/.bashrc"
            say "or ~/.profile) to keep it. For fish: fish_add_path \"$shown\"."
            ;;
    esac
    other=$(command -v fenec 2>/dev/null || true)
    if [ -n "$other" ] && [ "$other" != "$bindir/fenec" ]; then
        say ""
        say "Note: 'fenec' on your PATH is $other, not this one."
    fi

    say ""
    say "A first query: https://fenecdb.com/docs/quickstart"
}

main "$@"
