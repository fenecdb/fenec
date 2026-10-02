#!/bin/sh
# The Notes CLI's smoke test against this repository's build: the native
# library with its JNI functions, and the Kotlin library itself through a
# composite build (FENEC_LOCAL=1). Then, where an Android SDK is installed,
# the Compose app is built. Linux with Gradle and Java (CI) runs it here;
# elsewhere it runs in containers, as integrations/kotlin/run-tests.sh does.
#
#   examples/kotlin-android/run-smoke.sh
set -eu
ROOT=$(cd "$(dirname "$0")/../.." && pwd)

if [ "$(uname)" = Linux ] && command -v gradle >/dev/null 2>&1 && command -v java >/dev/null 2>&1; then
  CARGO=${CARGO:-$(test -x "$HOME/.cargo/bin/cargo" && echo "$HOME/.cargo/bin/cargo" || echo cargo)}
  (cd "$ROOT" && "$CARGO" rustc -q -p fenec-ffi --lib --features jni --profile ffi --crate-type cdylib)
  cd "$ROOT/examples/kotlin-android"
  FENEC_LOCAL=1 FENEC_LIBRARY="$ROOT/target/ffi/libfenec_ffi.so" gradle --no-daemon -q :cli:run --args=smoke
  if [ -n "${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}" ]; then
    FENEC_LOCAL=1 gradle --no-daemon -q :app:assembleDebug
  fi
  exit 0
fi

command -v docker >/dev/null 2>&1 || { echo "kotlin-android: needs Docker, or Linux with Gradle and Java" >&2; exit 1; }

# A target directory of its own: the host's target/ holds this machine's builds.
docker run --rm -v "$ROOT":/src -w /src \
  -v fenec-cargo-registry:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/src/target/linux-docker \
  rust:1-slim-bookworm \
  sh -c 'export RUSTUP_TOOLCHAIN=$(ls /usr/local/rustup/toolchains | head -1) &&
    cargo rustc -q -p fenec-ffi --lib --features jni --profile ffi --crate-type cdylib'

docker run --rm -v "$ROOT":/src -w /src/examples/kotlin-android \
  -v fenec-gradle:/home/gradle/.gradle \
  -e FENEC_LOCAL=1 \
  -e FENEC_LIBRARY=/src/target/linux-docker/ffi/libfenec_ffi.so \
  gradle:8-jdk17 \
  gradle --no-daemon -q :cli:run --args=smoke
