#!/bin/sh
# The Android AAR: crates/fenec-ffi with its JNI functions for each ABI,
# linked by the NDK's clang, then the `android` module's release build.
# Needs ANDROID_HOME (the SDK, Gradle's Android plugin reads it) and
# ANDROID_NDK_HOME (the NDK); GitHub's Ubuntu runners have both.
#
#   integrations/kotlin/build-aar.sh [out-dir]
#
# Prints each ABI's library size, stripped as the ffi profile strips it.
# The shared library alone (`cargo rustc --crate-type cdylib`): built beside
# the crate's static library and rlib, its link time optimisation left it
# 4% larger -- 1.58 MB against 1.51 on aarch64-apple-darwin.
set -eu
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
OUT=${1:-$ROOT/integrations/kotlin/build}
NDK=${ANDROID_NDK_HOME:?ANDROID_NDK_HOME names the NDK}
: "${ANDROID_HOME:?ANDROID_HOME names the SDK}"
CARGO=${CARGO:-$(test -x "$HOME/.cargo/bin/cargo" && echo "$HOME/.cargo/bin/cargo" || echo cargo)}
RUSTUP=${RUSTUP:-$(test -x "$HOME/.cargo/bin/rustup" && echo "$HOME/.cargo/bin/rustup" || echo rustup)}
case "$(uname)" in
  Darwin) HOST=darwin-x86_64 ;;
  *) HOST=linux-x86_64 ;;
esac
BIN="$NDK/toolchains/llvm/prebuilt/$HOST/bin"
API=21
JNI="$ROOT/integrations/kotlin/android/build/jniLibs"
rm -rf "$JNI"

# The Rust target, the ABI's directory, and the NDK's clang for it.
for row in \
  "aarch64-linux-android arm64-v8a aarch64-linux-android" \
  "armv7-linux-androideabi armeabi-v7a armv7a-linux-androideabi" \
  "x86_64-linux-android x86_64 x86_64-linux-android"; do
  set -- $row
  target=$1 abi=$2 clang=$3
  "$RUSTUP" target add "$target" >/dev/null 2>&1 || true
  env_target=$(echo "$target" | tr 'a-z-' 'A-Z_')
  env "CARGO_TARGET_${env_target}_LINKER=$BIN/${clang}${API}-clang" \
    "CC_${target}=$BIN/${clang}${API}-clang" \
    "AR_${target}=$BIN/llvm-ar" \
    "$CARGO" rustc -q -p fenec-ffi --lib --features jni --profile ffi --target "$target" --crate-type cdylib \
    --manifest-path "$ROOT/Cargo.toml"
  mkdir -p "$JNI/$abi"
  cp "$ROOT/target/$target/ffi/libfenec_ffi.so" "$JNI/$abi/"
  echo "$abi  $(wc -c < "$JNI/$abi/libfenec_ffi.so") bytes"
done

cd "$ROOT/integrations/kotlin"
gradle --no-daemon -q :android:assembleRelease
mkdir -p "$OUT"
version=$(sed -n 's/^ *version = "\(.*\)"/\1/p' build.gradle.kts)
cp android/build/outputs/aar/android-release.aar "$OUT/fenecdb-android-$version.aar"
echo "$OUT/fenecdb-android-$version.aar  $(wc -c < "$OUT/fenecdb-android-$version.aar") bytes"
