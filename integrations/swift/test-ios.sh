#!/bin/sh
# The Swift package's tests on an iOS simulator (CI's macOS runner, Xcode):
# the XCFramework's simulator slice linked into the test bundle and run
# there. Needs build-xcframework.sh to have built every slice.
set -eu
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
# The first iPhone the runner's Xcode has: their names change with each
# image.
device=$(xcrun simctl list devices available -j | python3 -c '
import json, sys
devices = json.load(sys.stdin)["devices"]
for runtime in sorted(devices, reverse=True):
    if "iOS" in runtime:
        for d in devices[runtime]:
            if d["name"].startswith("iPhone"):
                print(d["udid"]); sys.exit()
sys.exit("no iPhone simulator")
')
# SwiftPM's scheme for the package: its name, or with -Package where it
# has several products.
scheme=$(xcodebuild -list -json | python3 -c '
import json, sys
schemes = json.load(sys.stdin)["workspace"]["schemes"]
print("FenecDB-Package" if "FenecDB-Package" in schemes else "FenecDB")
')
# The result bundle where CI looks for it when a run fails (each test's
# output, the crash logs); xcodebuild refuses a path that exists.
RESULT="$ROOT/integrations/swift/build/ios-tests.xcresult"
rm -rf "$RESULT"
mkdir -p "$(dirname "$RESULT")"
xcodebuild test -quiet -scheme "$scheme" -destination "id=$device" -skipMacroValidation \
  -resultBundlePath "$RESULT"
