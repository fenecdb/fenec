#!/bin/sh
# The Kotlin library's JVM tests (`make kotlin-test`): the native library
# built for Linux with its JNI functions, then JUnit under Gradle -- the
# engine, the builder over every golden case, live queries as Flows.
#
# Where Gradle and Java are on the machine and it runs Linux (CI), both run
# here. Otherwise both run in containers -- rust:1-slim-bookworm builds the
# library for the container's Linux and gradle:8-jdk17 runs the tests --
# with Cargo's and Gradle's caches in volumes of their own.
set -eu
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
GOLDEN=integrations/builder-golden.json

if [ "$(uname)" = Linux ] && command -v gradle >/dev/null 2>&1 && command -v java >/dev/null 2>&1; then
  CARGO=${CARGO:-$(test -x "$HOME/.cargo/bin/cargo" && echo "$HOME/.cargo/bin/cargo" || echo cargo)}
  (cd "$ROOT" && "$CARGO" build -q -p fenec-ffi --features jni --profile ffi)
  cd "$ROOT/integrations/kotlin"
  FENEC_LIBRARY="$ROOT/target/ffi/libfenec_ffi.so" FENEC_GOLDEN="$ROOT/$GOLDEN" \
    exec gradle --no-daemon -q :fenecdb:test "$@"
fi

command -v docker >/dev/null 2>&1 || { echo "kotlin-test: needs Docker, or Linux with Gradle and Java" >&2; exit 1; }

# A target directory of its own: the host's target/ holds this machine's
# builds, and a Linux build in it would make Cargo build each again. The
# image's own toolchain, by name: rust-toolchain.toml's `stable` is not
# its name, and rustup installed a second one each run.
docker run --rm -v "$ROOT":/src -w /src \
  -v fenec-cargo-registry:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/src/target/linux-docker \
  rust:1-slim-bookworm \
  sh -c 'RUSTUP_TOOLCHAIN=$(ls /usr/local/rustup/toolchains | head -1) cargo build -q -p fenec-ffi --features jni --profile ffi'

docker run --rm -v "$ROOT":/src -w /src/integrations/kotlin \
  -v fenec-gradle:/home/gradle/.gradle \
  -e FENEC_LIBRARY=/src/target/linux-docker/ffi/libfenec_ffi.so \
  -e FENEC_GOLDEN=/src/$GOLDEN \
  gradle:8-jdk17 \
  gradle --no-daemon -q :fenecdb:test "$@"
