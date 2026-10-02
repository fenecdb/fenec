#!/bin/sh
# Points Package.swift's binary target at a release's XCFramework: its
# version (the release's download URL) and the zip's checksum, which
# SwiftPM holds the download to. What .github/workflows/swift-binary.yml
# writes before a release is tagged -- the tag has to name a manifest that
# already holds the checksum of the zip the release will attach.
#
#   integrations/swift/set-binary.sh 0.2.0 <checksum>
set -eu
[ $# -eq 2 ] || { echo "usage: $0 X.Y.Z checksum" >&2; exit 2; }
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
python3 - "$ROOT/Package.swift" "$1" "$2" <<'EOF'
import re, sys
path, version, checksum = sys.argv[1:]
if not re.fullmatch(r"\d+\.\d+\.\d+", version) or not re.fullmatch(r"[0-9a-f]{64}", checksum):
    sys.exit("a version X.Y.Z and a 64-digit hex checksum")
text = open(path).read()
text, n = re.subn(r'^let release = "[^"]*"', f'let release = "{version}"', text, flags=re.M)
text, m = re.subn(r'^let checksum = "[^"]*"', f'let checksum = "{checksum}"', text, flags=re.M)
if n != 1 or m != 1:
    sys.exit("Package.swift: no `let release` or `let checksum` line")
open(path, "w").write(text)
print(f"Package.swift: FenecFFI {version}, checksum {checksum}")
EOF
