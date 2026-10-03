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
# The server the sync tests start, kill and start again.
CARGO=${CARGO:-$(test -x "$HOME/.cargo/bin/cargo" && echo "$HOME/.cargo/bin/cargo" || echo cargo)}
(cd "$ROOT" && "$CARGO" build -q -p fenec-server)
cd "$ROOT"
DEV=$(xcode-select -p 2>/dev/null || true)
case "$DEV" in
  */CommandLineTools)
    F="$DEV/Library/Developer/Frameworks"
    swift test -Xswiftc -F -Xswiftc "$F" -Xlinker -F -Xlinker "$F" -Xlinker -rpath -Xlinker "$F" "$@"
    ;;
  *)
    swift test "$@"
    ;;
esac
# Again with Swift's cooperative pool cut to one thread, where a call that
# blocks a thread of it hangs at once rather than now and then on a machine
# of few cores (test-strict.sh).
"$ROOT/integrations/swift/test-strict.sh"
