#!/bin/sh
# Puts the native library where each platform's project takes it from:
# the dynamic XCFramework integrations/swift/build-xcframework.sh --dynamic
# made (FenecFFI.framework a slice) into
# ios/Frameworks and macos/Frameworks, and the .so for each ABI
# integrations/kotlin/build-aar.sh made into android/src/main/jniLibs.
# Whichever of them is built is copied; a release builds both first.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
XC="$ROOT/integrations/swift/build/FenecFFIDynamic.xcframework"
JNI="$ROOT/integrations/kotlin/android/build/jniLibs"

if [ -d "$XC" ]; then
  for p in ios macos; do
    rm -rf "$HERE/$p/Frameworks"
    mkdir -p "$HERE/$p/Frameworks"
    # -R copies a macOS framework's links as links.
    cp -R "$XC" "$HERE/$p/Frameworks/"
  done
  echo "FenecFFIDynamic.xcframework -> ios/Frameworks, macos/Frameworks"
fi
if [ -d "$JNI" ]; then
  rm -rf "$HERE/android/src/main/jniLibs"
  cp -R "$JNI" "$HERE/android/src/main/jniLibs"
  echo "jniLibs -> android/src/main/jniLibs: $(ls "$HERE/android/src/main/jniLibs" | tr '\n' ' ')"
fi
