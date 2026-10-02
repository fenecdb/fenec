#!/bin/sh
# The Dart package's tests (`make dart-test`): the native library built for
# this machine, and the fenec-server the sync tests start, then `dart test`
# against them -- the engine, the builder over every golden case, live
# queries as Streams, a replica against a real server -- and, where Flutter
# is installed, the Flutter plugin's own test the same way.
set -eu
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
CARGO=${CARGO:-$(test -x "$HOME/.cargo/bin/cargo" && echo "$HOME/.cargo/bin/cargo" || echo cargo)}
(cd "$ROOT" && "$CARGO" rustc -q -p fenec-ffi --lib --profile ffi --crate-type cdylib)
# The server the sync tests start, kill and start again.
(cd "$ROOT" && "$CARGO" build -q -p fenec-server)
case "$(uname)" in
  Darwin) LIB="$ROOT/target/ffi/libfenec_ffi.dylib" ;;
  *) LIB="$ROOT/target/ffi/libfenec_ffi.so" ;;
esac
export FENEC_LIBRARY="$LIB"
export FENEC_SERVER="$ROOT/target/debug/fenec-server"
export FENEC_GOLDEN="$ROOT/integrations/builder-golden.json"

cd "$ROOT/integrations/dart/fenecdb"
dart pub get >/dev/null
dart format --output=none --set-exit-if-changed .
dart analyze --fatal-infos
dart test "$@"

if command -v flutter >/dev/null 2>&1; then
  cd "$ROOT/integrations/dart/fenecdb_flutter"
  flutter pub get >/dev/null
  flutter analyze lib test
  flutter test
else
  echo "flutter not found: the Flutter plugin (integrations/dart/fenecdb_flutter) not tested"
fi
