#!/bin/sh
# The native library's size for each target given, as an app ships it: the
# shared library linked and stripped in the ffi profile (the static one an
# XCFramework holds is the objects before the app's linker takes what it
# uses). A Markdown table on stdout, for the job's summary.
#
#   integrations/ffi-sizes.sh aarch64-apple-ios x86_64-unknown-linux-gnu ...
#
# Android's targets want the NDK's clang as their linker (ANDROID_NDK_HOME),
# Apple's the SDKs of an installed Xcode.
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
CARGO=${CARGO:-$(test -x "$HOME/.cargo/bin/cargo" && echo "$HOME/.cargo/bin/cargo" || echo cargo)}
echo "| target | stripped shared library |"
echo "| --- | ---: |"
for t in "$@"; do
  envs=""
  case "$t" in
    *android*)
      host=$(uname | tr 'A-Z' 'a-z')-x86_64
      clang=$(echo "$t" | sed 's/^armv7-/armv7a-/')
      envs="CARGO_TARGET_$(echo "$t" | tr 'a-z-' 'A-Z_')_LINKER=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/$host/bin/${clang}21-clang"
      ;;
  esac
  feature=""
  case "$t" in *android*|*linux*) feature="--features jni" ;; esac
  # shellcheck disable=SC2086
  (cd "$ROOT" && env $envs "$CARGO" rustc -q -p fenec-ffi --lib --profile ffi --target "$t" $feature --crate-type cdylib) >&2
  lib=$(ls "$ROOT/target/$t/ffi/libfenec_ffi.so" "$ROOT/target/$t/ffi/libfenec_ffi.dylib" 2>/dev/null | head -1)
  bytes=$(wc -c < "$lib" | tr -d ' ')
  echo "| \`$t\` | $(python3 -c "print(f'{$bytes / 1e6:.2f} MB ($bytes bytes)')") |"
done
