#!/bin/sh
# crates/fenec-ffi as an XCFramework for each Apple platform, in
# integrations/swift/build/:
#
#   FenecFFI.xcframework         a static library a slice, with the header and
#                                a module map: what Package.swift takes in
#                                place of the release's download
#   dynamic/FenecFFI.xcframework  (--dynamic, zipped as FenecFFIDynamic) FenecFFI.framework a slice, the
#                                shared library: what the Flutter plugin's
#                                pods vendor
#
#   integrations/swift/build-xcframework.sh            macOS, iOS, the simulator
#   integrations/swift/build-xcframework.sh --macos    macOS alone, this machine's arch
#   integrations/swift/build-xcframework.sh --dynamic  the frameworks instead
#   integrations/swift/build-xcframework.sh --zip      and the zip a release
#                                                      attaches (the static one
#                                                      with its checksum)
#
# Assembled by hand -- the Info.plist is a list of the slices -- rather
# than by `xcodebuild -create-xcframework`, which needs Xcode: the
# Command Line Tools build and test the macOS slices with this alone. Rust
# builds a static library without linking it, so the static iOS slices
# build without the iOS SDK too; the dynamic ones are linked, and need it.
#
# Why two. Swift links the static library into the app, which is what an
# app ships. The Flutter plugin's Dart side finds the functions by name at
# run time, where nothing in the app calls them, so a static library had to
# be kept whole with -force_load at the app's link -- and that pointed the
# Runner at a file CocoaPods' "Copy XCFrameworks" phase makes with no order
# declared against it: `flutter build ios` stopped at "Build input file
# cannot be found". A dynamic framework is what Flutter's own docs give an
# FFI plugin with a prebuilt binary: CocoaPods embeds it, nothing is
# force-loaded, and Dart opens `FenecFFI.framework/FenecFFI`.
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
OUT="$ROOT/integrations/swift/build"
CARGO=${CARGO:-$(test -x "$HOME/.cargo/bin/cargo" && echo "$HOME/.cargo/bin/cargo" || echo cargo)}
RUSTUP=${RUSTUP:-$(test -x "$HOME/.cargo/bin/rustup" && echo "$HOME/.cargo/bin/rustup" || echo rustup)}
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)
# The platforms Package.swift and the pods declare: the objects say so too,
# rather than Rust's own defaults.
export IPHONEOS_DEPLOYMENT_TARGET=15.0 MACOSX_DEPLOYMENT_TARGET=12.0

macos_only=false
zip=false
dynamic=false
for a in "$@"; do
  case "$a" in
    --macos) macos_only=true ;;
    --zip) zip=true ;;
    --dynamic) dynamic=true ;;
    *) echo "usage: $0 [--macos] [--dynamic] [--zip]" >&2; exit 2 ;;
  esac
done
# CocoaPods links a vendored XCFramework by its own name, so the dynamic one
# is called FenecFFI.xcframework too, as the frameworks inside it are --
# named FenecFFIDynamic, the app's link asked for a framework of that name.
# It is built in a directory of its own beside the static one, and only its
# zip carries the other name.
NAME=FenecFFI
if $dynamic; then XDIR="$OUT/dynamic"; ZIPNAME=FenecFFIDynamic; else XDIR="$OUT"; ZIPNAME=FenecFFI; fi
mkdir -p "$XDIR"
XC="$XDIR/$NAME.xcframework"

# One static library a target, the lib alone: `cargo build` would link the
# cdylib too, which for iOS needs its SDK.
static_lib() {
  "$RUSTUP" target add "$1" >/dev/null 2>&1 || true
  (cd "$ROOT" && "$CARGO" rustc -q -p fenec-ffi --lib --profile ffi --target "$1" --crate-type staticlib)
  echo "$ROOT/target/$1/ffi/libfenec_ffi.a"
}

# One shared library a target, its install name the framework's binary, so
# the app's loader finds it through @rpath in its Frameworks directory.
shared_lib() {
  "$RUSTUP" target add "$1" >/dev/null 2>&1 || true
  (cd "$ROOT" && "$CARGO" rustc -q -p fenec-ffi --lib --profile ffi --target "$1" --crate-type cdylib \
    -- -C "link-arg=-Wl,-install_name,$2")
  echo "$ROOT/target/$1/ffi/libfenec_ffi.dylib"
}

# A static slice: the library (several architectures made one with lipo),
# the header and the module map Swift imports it by.
static_slice() {
  id=$1
  shift
  mkdir -p "$XC/$id/Headers"
  libs=""
  for t in "$@"; do libs="$libs $(static_lib "$t")"; done
  # shellcheck disable=SC2086
  lipo -create $libs -output "$XC/$id/libfenec_ffi.a"
  cp "$ROOT/crates/fenec-ffi/include/fenec.h" "$XC/$id/Headers/"
  cat > "$XC/$id/Headers/module.modulemap" <<'EOF'
module FenecFFI {
    header "fenec.h"
    export *
}
EOF
  echo "$id  $(wc -c < "$XC/$id/libfenec_ffi.a") bytes (static, unlinked)" >&2
}

# A framework's Info.plist: the bundle's name, its binary and the lowest
# system it runs on, which App Store validation reads.
framework_plist() {
  platform=$1 minimum=$2
  cat <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleExecutable</key>
	<string>FenecFFI</string>
	<key>CFBundleIdentifier</key>
	<string>com.fenecdb.FenecFFI</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>FenecFFI</string>
	<key>CFBundlePackageType</key>
	<string>FMWK</string>
	<key>CFBundleShortVersionString</key>
	<string>$VERSION</string>
	<key>CFBundleVersion</key>
	<string>$VERSION</string>
	<key>CFBundleSupportedPlatforms</key>
	<array>
		<string>$platform</string>
	</array>
	$minimum
</dict>
</plist>
EOF
}

# A dynamic slice: FenecFFI.framework. iOS's is flat, the binary and the
# Info.plist side by side; macOS's is versioned (Versions/A, Current and
# the links to it), which code signing there requires.
dynamic_slice() {
  id=$1 platform=$2
  shift 2
  fw="$XC/$id/FenecFFI.framework"
  case "$platform" in
    MacOSX)
      mkdir -p "$fw/Versions/A/Resources"
      install="@rpath/FenecFFI.framework/Versions/A/FenecFFI"
      bin="$fw/Versions/A/FenecFFI"
      plist="$fw/Versions/A/Resources/Info.plist"
      minimum="<key>LSMinimumSystemVersion</key>
	<string>$MACOSX_DEPLOYMENT_TARGET</string>"
      ;;
    *)
      mkdir -p "$fw"
      install="@rpath/FenecFFI.framework/FenecFFI"
      bin="$fw/FenecFFI"
      plist="$fw/Info.plist"
      minimum="<key>MinimumOSVersion</key>
	<string>$IPHONEOS_DEPLOYMENT_TARGET</string>"
      ;;
  esac
  libs=""
  for t in "$@"; do libs="$libs $(shared_lib "$t" "$install")"; done
  # shellcheck disable=SC2086
  lipo -create $libs -output "$bin"
  framework_plist "$platform" "$minimum" > "$plist"
  if [ "$platform" = MacOSX ]; then
    ln -s A "$fw/Versions/Current"
    ln -s Versions/Current/FenecFFI "$fw/FenecFFI"
    ln -s Versions/Current/Resources "$fw/Resources"
  fi
  echo "$id  $(wc -c < "$bin") bytes (FenecFFI.framework)" >&2
}

slice() {
  if $dynamic; then dynamic_slice "$@"; else
    id=$1
    shift 2
    static_slice "$id" "$@"
  fi
}

# One entry of Info.plist's AvailableLibraries.
entry() {
  id=$1 platform=$2 variant=$3
  shift 3
  if $dynamic; then
    lib=FenecFFI.framework
    case "$platform" in
      macos) binary=FenecFFI.framework/Versions/A/FenecFFI ;;
      *) binary=FenecFFI.framework/FenecFFI ;;
    esac
  else
    lib=libfenec_ffi.a binary=libfenec_ffi.a
  fi
  printf '\t\t<dict>\n'
  printf '\t\t\t<key>BinaryPath</key>\n\t\t\t<string>%s</string>\n' "$binary"
  if ! $dynamic; then
    printf '\t\t\t<key>HeadersPath</key>\n\t\t\t<string>Headers</string>\n'
  fi
  printf '\t\t\t<key>LibraryIdentifier</key>\n\t\t\t<string>%s</string>\n' "$id"
  printf '\t\t\t<key>LibraryPath</key>\n\t\t\t<string>%s</string>\n' "$lib"
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
  slice "macos-$arch" MacOSX "$host"
  entries=$(entry "macos-$arch" macos "" "$arch")
else
  slice macos-arm64_x86_64 MacOSX aarch64-apple-darwin x86_64-apple-darwin
  slice ios-arm64 iPhoneOS aarch64-apple-ios
  slice ios-arm64_x86_64-simulator iPhoneSimulator aarch64-apple-ios-sim x86_64-apple-ios
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
  # The zip a release attaches. -X leaves out the extra attributes, and -y
  # keeps a macOS framework's links as links. The static one's checksum is
  # what Package.swift names it by; the dynamic one is the Flutter plugin's
  # and named by nothing.
  rm -f "$OUT/$ZIPNAME.xcframework.zip"
  (cd "$XDIR" && zip -qrXy "$OUT/$ZIPNAME.xcframework.zip" "$NAME.xcframework")
  echo "$OUT/$ZIPNAME.xcframework.zip  $(wc -c < "$OUT/$ZIPNAME.xcframework.zip") bytes"
  if ! $dynamic; then
    sum=$(swift package compute-checksum "$OUT/$ZIPNAME.xcframework.zip")
    echo "$sum" > "$OUT/$ZIPNAME.xcframework.zip.checksum"
    echo "checksum $sum"
  fi
fi
