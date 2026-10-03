#!/bin/sh
# The Notes app against this repository's build rather than pub.dev's:
# pubspec_overrides.yaml (ignored by git) points fenecdb and fenecdb_flutter
# at integrations/dart, and the smoke steps run under `flutter test` with
# the native library built for this machine. Without Flutter, the same
# steps run under plain Dart (test/smoke.dart), the widgets left out.
#
#   examples/flutter/run-tests.sh            the smoke steps
#   examples/flutter/run-tests.sh apk        and the app built for Android
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
CARGO=${CARGO:-$(test -x "$HOME/.cargo/bin/cargo" && echo "$HOME/.cargo/bin/cargo" || echo cargo)}
(cd "$ROOT" && "$CARGO" rustc -q -p fenec-ffi --lib --profile ffi --crate-type cdylib)
case "$(uname)" in
  Darwin) export FENEC_LIBRARY="$ROOT/target/ffi/libfenec_ffi.dylib" ;;
  *) export FENEC_LIBRARY="$ROOT/target/ffi/libfenec_ffi.so" ;;
esac

cat >"$HERE/pubspec_overrides.yaml" <<EOF
dependency_overrides:
  fenecdb:
    path: $ROOT/integrations/dart/fenecdb
  fenecdb_flutter:
    path: $ROOT/integrations/dart/fenecdb_flutter
EOF

cd "$HERE"
if command -v flutter >/dev/null 2>&1; then
  flutter pub get >/dev/null
  flutter analyze lib test
  flutter test
  if [ "${1:-}" = apk ]; then
    # The plugin's jniLibs, from integrations/kotlin/build-aar.sh's build.
    "$ROOT/integrations/dart/fenecdb_flutter/prepare.sh"
    flutter create --platforms=android --project-name fenecdb_notes . >/dev/null
    rm -rf test/widget_test.dart
    flutter build apk --debug
    unzip -l build/app/outputs/flutter-apk/app-debug.apk | grep -q libfenec_ffi.so
  fi
else
  # Plain Dart: the app's data layer and the smoke steps, in a package of
  # their own beside it (Flutter's SDK is what pub would want otherwise).
  command -v dart >/dev/null 2>&1 || { echo "flutter and dart not found -- Flutter's example skipped"; exit 0; }
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  mkdir -p "$tmp/lib" "$tmp/test"
  cp lib/notes.dart "$tmp/lib/"
  cp test/smoke.dart "$tmp/test/"
  cp schema.fenecql "$tmp/"
  printf 'name: fenecdb_notes\nenvironment:\n  sdk: ^3.4.0\ndependencies:\n  fenecdb:\n    path: %s\n' \
    "$ROOT/integrations/dart/fenecdb" >"$tmp/pubspec.yaml"
  echo "flutter not found -- the smoke steps under plain Dart, without the widgets"
  (cd "$tmp" && dart pub get >/dev/null && dart analyze --fatal-infos lib test && dart run test/smoke.dart)
fi
