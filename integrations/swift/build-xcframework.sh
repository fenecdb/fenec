#!/bin/sh
# FenecFFI.xcframework: crates/fenec-ffi as a static library for each Apple
# platform, with its header and a module map, in
# integrations/swift/build/ -- where Package.swift takes it from in place of
# the release's download.
#
#   integrations/swift/build-xcframework.sh            macOS, iOS, the simulator
#   integrations/swift/build-xcframework.sh --macos    macOS alone (swift test)
#   integrations/swift/build-xcframework.sh --zip      and the zip a release
#                                                      attaches, with its checksum
#
# Assembled by hand -- the Info.plist is a list of the slices -- rather
# than by `xcodebuild -create-xcframework`, which needs Xcode: the
# Command Line Tools build and test the macOS slice with this alone. A
# static library, since an app links what it ships and an iOS app may not
# load a library from outside its bundle anyway. Rust builds a static
# library without linking it, so the iOS slices build without the iOS SDK
# too; testing them needs a simulator, which CI has.
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
OUT="$ROOT/integrations/swift/build"
XC="$OUT/FenecFFI.xcframework"
CARGO=${CARGO:-$(test -x "$HOME/.cargo/bin/cargo" && echo "$HOME/.cargo/bin/cargo" || echo cargo)}
RUSTUP=${RUSTUP:-$(test -x "$HOME/.cargo/bin/rustup" && echo "$HOME/.cargo/bin/rustup" || echo rustup)}

macos_only=false
zip=false
for a in "$@"; do
  case "$a" in
    --macos) macos_only=true ;;
    --zip) zip=true ;;
    *) echo "usage: $0 [--macos] [--zip]" >&2; exit 2 ;;
  esac
done

# One static library a target, the lib alone: `cargo build` would link the
# cdylib too, which for iOS needs its SDK.
build() {
  "$RUSTUP" target add "$1" >/dev/null 2>&1 || true
  (cd "$ROOT" && "$CARGO" rustc -q -p fenec-ffi --lib --profile ffi --target "$1" --crate-type staticlib)
  echo "$ROOT/target/$1/ffi/libfenec_ffi.a"
}

# A slice: the library (several architectures made one with lipo), the
# header and the module map Swift imports it by.
slice() {
  id=$1
  shift
  mkdir -p "$XC/$id/Headers"
  libs=""
  for t in "$@"; do libs="$libs $(build "$t")"; done
  # shellcheck disable=SC2086
  lipo -create $libs -output "$XC/$id/libfenec_ffi.a"
  cp "$ROOT/crates/fenec-ffi/include/fenec.h" "$XC/$id/Headers/"
  cat > "$XC/$id/Headers/module.modulemap" <<'EOF'
module FenecFFI {
    header "fenec.h"
    export *
}
EOF
  echo "$id  $(wc -c < "$XC/$id/libfenec_ffi.a") bytes (static, unlinked)"
}

# One entry of Info.plist's AvailableLibraries.
entry() {
  id=$1 platform=$2 variant=$3
  shift 3
  printf '\t\t<dict>\n'
  printf '\t\t\t<key>BinaryPath</key>\n\t\t\t<string>libfenec_ffi.a</string>\n'
  printf '\t\t\t<key>HeadersPath</key>\n\t\t\t<string>Headers</string>\n'
  printf '\t\t\t<key>LibraryIdentifier</key>\n\t\t\t<string>%s</string>\n' "$id"
  printf '\t\t\t<key>LibraryPath</key>\n\t\t\t<string>libfenec_ffi.a</string>\n'
  printf '\t\t\t<key>SupportedArchitectures</key>\n\t\t\t<array>\n'
  for arch in "$@"; do printf '\t\t\t\t<string>%s</string>\n' "$arch"; done
  printf '\t\t\t</array>\n'
  printf '\t\t\t<key>SupportedPlatform</key>\n\t\t\t<string>%s</string>\n' "$platform"
  if [ -n "$variant" ]; then
    printf '\t\t\t<key>SupportedPlatformVariant</key>\n\t\t\t<string>%s</string>\n' "$variant"
  fi
  printf '\t\t</dict>\n'
}

rm -rf "$XC"
mkdir -p "$XC"
if $macos_only; then
  # This machine's architecture alone: what `swift test` links.
  case "$(uname -m)" in
    arm64) host=aarch64-apple-darwin arch=arm64 ;;
    *) host=x86_64-apple-darwin arch=x86_64 ;;
  esac
  slice "macos-$arch" "$host"
  entries=$(entry "macos-$arch" macos "" "$arch")
else
  slice macos-arm64_x86_64 aarch64-apple-darwin x86_64-apple-darwin
  slice ios-arm64 aarch64-apple-ios
  slice ios-arm64_x86_64-simulator aarch64-apple-ios-sim x86_64-apple-ios
  entries=$(
    entry macos-arm64_x86_64 macos "" arm64 x86_64
    entry ios-arm64 ios "" arm64
    entry ios-arm64_x86_64-simulator ios simulator arm64 x86_64
  )
fi

cat > "$XC/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>AvailableLibraries</key>
	<array>
$entries
	</array>
	<key>CFBundlePackageType</key>
	<string>XFWK</string>
	<key>XCFrameworkFormatVersion</key>
	<string>1.0</string>
</dict>
</plist>
EOF
echo "$XC"

if $zip; then
  # The zip a release attaches and Package.swift names by URL and checksum.
  # -X leaves out the extra attributes, so the bytes are the files'.
  rm -f "$OUT/FenecFFI.xcframework.zip"
  (cd "$OUT" && zip -qrX FenecFFI.xcframework.zip FenecFFI.xcframework)
  sum=$(swift package compute-checksum "$OUT/FenecFFI.xcframework.zip")
  echo "$sum" > "$OUT/FenecFFI.xcframework.zip.checksum"
  echo "$OUT/FenecFFI.xcframework.zip  $(wc -c < "$OUT/FenecFFI.xcframework.zip") bytes, checksum $sum"
fi
