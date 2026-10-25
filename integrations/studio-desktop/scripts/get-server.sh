#!/bin/sh
# Puts a fenec-server that has --studio at bin/fenec-server, where
# electrobun.config.ts copies it into the app bundle.
#
# The v0.1.10 release asset has no --studio (it is older than the studio), so
# until a release carries it this builds the checkout the app sits in.
set -eu
cd "$(dirname "$0")/.."
root=$(cd ../.. && pwd)
mkdir -p bin
if [ -n "${FENEC_SERVER_FROM:-}" ]; then
  cp "$FENEC_SERVER_FROM" bin/fenec-server
else
  (cd "$root" && cargo build --release -p fenec-server)
  cp "$root/target/release/fenec-server" bin/fenec-server
fi
./bin/fenec-server --help 2>&1 | grep -q -- '--studio' || { echo "bin/fenec-server has no --studio" >&2; exit 1; }
ls -l bin/fenec-server
