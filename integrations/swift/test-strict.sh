#!/bin/sh
# The Swift package's tests again, as `swift test` built them, with Swift's
# cooperative pool cut to one thread (LIBDISPATCH_COOPERATIVE_POOL_STRICT=1):
# a thread of the pool blocked waiting for work that needs the pool -- a
# semaphore, a `queue.sync`, a task's result waited for -- hangs here at
# once, where with a thread a core it hangs only when every thread is
# taken, now and then on GitHub's three-core macOS runner or a phone. The
# tests' server helper blocked threads of the pool (`waitUntilExit()`,
# which now and then never came back) and held a runner for six hours.
#
# Run after `swift test` (run-tests.sh does). The variable cannot go through
# `swift test`: SwiftPM is a Swift program too, and with one thread in its
# own pool it stalls before it runs anything. So the built bundle is handed
# to the toolchain's swift-testing runner, as `swift test` hands it.
#
# A run that has not ended after FENEC_STRICT_TIMEOUT seconds (300) is
# sampled -- every thread's stack, to the log -- and fails.
set -eu
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
BIN=$(swift build --show-bin-path)
BUNDLE="$BIN/FenecDBPackageTests.xctest/Contents/MacOS/FenecDBPackageTests"
test -x "$BUNDLE" || { echo "no test bundle at $BUNDLE: run swift test first" >&2; exit 2; }
HELPER="$(dirname "$(xcrun --find swift)")/../libexec/swift/pm/swiftpm-testing-helper"
test -x "$HELPER" || { echo "no swiftpm-testing-helper beside $(xcrun --find swift)" >&2; exit 2; }

# Where Testing.framework is, as `swift test` tells the bundle: Xcode's
# platform (its libraries beside), or the Command Line Tools'.
FW=""
LIB=""
DEV=$(xcode-select -p 2>/dev/null || true)
PLATFORM=$(xcrun --show-sdk-platform-path 2>/dev/null || true)
for d in "$PLATFORM/Developer/Library/Frameworks" "$DEV/Library/Developer/Frameworks"; do
  if [ -d "$d/Testing.framework" ]; then FW="${FW:+$FW:}$d"; fi
done
if [ -n "$PLATFORM" ] && [ -d "$PLATFORM/Developer/usr/lib" ]; then LIB="$PLATFORM/Developer/usr/lib"; fi

LOG=$(mktemp -t fenecdb-strict)
LIBDISPATCH_COOPERATIVE_POOL_STRICT=1 DYLD_FRAMEWORK_PATH="$FW" DYLD_LIBRARY_PATH="$LIB" \
  "$HELPER" --test-bundle-path "$BUNDLE" --testing-library swift-testing >"$LOG" 2>&1 &
pid=$!
limit=${FENEC_STRICT_TIMEOUT:-300}
t=0
while kill -0 "$pid" 2>/dev/null && [ "$t" -lt "$limit" ]; do
  sleep 1
  t=$((t + 1))
done
if kill -0 "$pid" 2>/dev/null; then
  cat "$LOG"
  echo "the tests hung under one cooperative thread; every thread's stack:" >&2
  sample "$pid" 3 2>/dev/null || true
  # The servers the sync tests started.
  pkill -9 -P "$pid" 2>/dev/null || true
  kill -9 "$pid" 2>/dev/null || true
  rm -f "$LOG"
  exit 1
fi
set +e
wait "$pid"
code=$?
set -e
cat "$LOG"
rm -f "$LOG"
exit "$code"
