#!/bin/sh
# The Swift package's tests (`make swift-test`): the macOS slice of the
# XCFramework built for this machine, then `swift test` at the repository's
# root, where Package.swift is.
#
# With the Command Line Tools alone -- no Xcode -- swift-testing's
# framework is there but not on the compiler's or the linker's search
# path, so both are pointed at it.
set -eu
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
"$ROOT/integrations/swift/build-xcframework.sh" --macos
cd "$ROOT"
DEV=$(xcode-select -p 2>/dev/null || true)
case "$DEV" in
  */CommandLineTools)
    F="$DEV/Library/Developer/Frameworks"
    exec swift test -Xswiftc -F -Xswiftc "$F" -Xlinker -F -Xlinker "$F" -Xlinker -rpath -Xlinker "$F" "$@"
    ;;
  *)
    exec swift test "$@"
    ;;
esac
