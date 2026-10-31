#!/bin/sh
# Puts the fenec-server the app bundles at bin/fenec-server, and the `fenec`
# shell it takes, verifies and restores backups with at bin/fenec, where
# electrobun.config.ts copies them into the app.
#
#   scripts/get-server.sh           build them from this checkout
#                                   (cargo build --release -p fenec-server,
#                                   cargo build --profile cli -p fenec-cli)
#   scripts/get-server.sh 0.1.12    download release v0.1.12's asset for this
#                                   machine, which holds both, and check its
#                                   SHA-256 against the release's own SHA256SUMS
#
# Either way the server must have the two flags the app starts it with:
# --studio, and --exit-on-stdin-close, by which it stops when the app that holds
# the pipe on its stdin is gone, however it ended. A release older than those
# is refused here, not found out by a window that never opens: the first with
# --studio is 0.1.11, and the first with both is 0.1.12. And the shell must
# have `fenec verify` and --key-file, which the backups use.
#
#   CARGO               the cargo to build with (the Makefile's: ~/.cargo/bin/cargo
#                       where it exists, which has the wasm32 target the repository's
#                       rust-toolchain.toml asks for, else the one on PATH)
#   FENEC_RELEASES      where releases are downloaded from (a test points it at a
#                       directory with file://), default
#                       https://github.com/fenecdb/fenecdb/releases/download
#   FENEC_SERVER_OUT    where the server goes, default bin/fenec-server
#   FENEC_CLI_OUT       where the shell goes, default `fenec` beside the server
#   TARGET_TRIPLE       the release asset's target, default this machine's
set -eu
cd "$(dirname "$0")/.."
root=$(cd ../.. && pwd)

die() {
  echo "get-server: $*" >&2
  exit 1
}

out=${FENEC_SERVER_OUT:-bin/fenec-server}
cli_out=${FENEC_CLI_OUT:-$(dirname "$out")/fenec}
mkdir -p "$(dirname "$out")" "$(dirname "$cli_out")"
# Written beside the targets and moved into place only once both have passed,
# so a refused server or shell never replaces one that works.
stage="$out.new"
cli_stage="$cli_out.new"
rm -f "$stage" "$cli_stage"
tmp=
cleanup() {
  rm -f "$stage" "$cli_stage"
  [ -z "$tmp" ] || rm -rf "$tmp"
}
trap cleanup EXIT

version=${1:-}
version=${version#v}

if [ -z "$version" ]; then
  if [ -z "${CARGO:-}" ]; then
    if [ -x "$HOME/.cargo/bin/cargo" ]; then CARGO="$HOME/.cargo/bin/cargo"; else CARGO=cargo; fi
  fi
  # The shell in the profile a release builds it with (panic = abort), as the
  # release's own tarball carries it.
  (cd "$root" && "$CARGO" build --release -p fenec-server && "$CARGO" build --profile cli -p fenec-cli)
  cp "$root/target/release/fenec-server" "$stage"
  cp "$root/target/cli/fenec" "$cli_stage"
else
  # The version goes into a URL and a file name: only what a release is called.
  case "$version" in
    *[!0-9A-Za-z.-]* | "" | .* | -*) die "\"$version\" is not a release version (0.1.12, or v0.1.12)" ;;
  esac
  triple=${TARGET_TRIPLE:-}
  if [ -z "$triple" ]; then
    case "$(uname -s)-$(uname -m)" in
      Darwin-arm64) triple=aarch64-apple-darwin ;;
      Darwin-x86_64) triple=x86_64-apple-darwin ;;
      Linux-x86_64) triple=x86_64-unknown-linux-musl ;;
      Linux-aarch64 | Linux-arm64) triple=aarch64-unknown-linux-musl ;;
      *) die "no release asset for $(uname -s) $(uname -m); set TARGET_TRIPLE" ;;
    esac
  fi
  base=${FENEC_RELEASES:-https://github.com/fenecdb/fenecdb/releases/download}/v$version
  asset=fenec-v$version-$triple.tar.gz
  tmp=$(mktemp -d "${TMPDIR:-/tmp}/fenec-server.XXXXXX")
  curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" || die "could not download $base/SHA256SUMS (is there a release v$version?)"
  curl -fsSL "$base/$asset" -o "$tmp/$asset" || die "could not download $base/$asset"
  # `sha256sum`'s lines: "<hash>  <file>". The asset must be listed: a release
  # that does not name it is not one to trust a download of.
  want=$(awk -v f="$asset" '$2 == f { print $1 }' "$tmp/SHA256SUMS")
  [ -n "$want" ] || die "the release's SHA256SUMS does not list $asset"
  if command -v sha256sum >/dev/null 2>&1; then
    got=$(sha256sum "$tmp/$asset" | cut -d' ' -f1)
  else
    got=$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)
  fi
  [ "$got" = "$want" ] || die "SHA-256 of $asset is $got, the release says $want: not using it"
  tar -xzf "$tmp/$asset" -C "$tmp" "fenec-v$version-$triple/fenec-server" || die "$asset holds no fenec-server"
  tar -xzf "$tmp/$asset" -C "$tmp" "fenec-v$version-$triple/fenec" || die "$asset holds no fenec shell"
  cp "$tmp/fenec-v$version-$triple/fenec-server" "$stage"
  cp "$tmp/fenec-v$version-$triple/fenec" "$cli_stage"
  echo "get-server: $asset, SHA-256 $got matches the release's SHA256SUMS"
fi

chmod +x "$stage" "$cli_stage"
help=$("$stage" --help 2>&1 || true)
for flag in --studio --exit-on-stdin-close; do
  printf '%s\n' "$help" | grep -q -- "$flag" ||
    die "this fenec-server has no $flag, which the app starts it with (the first release with --studio is 0.1.11, and with both flags 0.1.12): build the one in this checkout with scripts/get-server.sh, or name a release that has both"
done
cli_help=$("$cli_stage" backup --help 2>&1 || true)
for word in "fenec verify" --key-file; do
  printf '%s\n' "$cli_help" | grep -q -- "$word" ||
    die "this fenec shell has no \`$word\`, which the app's backups use: build the one in this checkout with scripts/get-server.sh, or name a release that has it"
done
mv "$stage" "$out"
mv "$cli_stage" "$cli_out"
ls -l "$out" "$cli_out"
