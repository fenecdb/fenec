#!/bin/sh
# Trellis end to end, as CI runs it: lint, then the tests -- auth, tokens,
# the role matrix, scoped subscriptions and search, a tenant moved and a
# node killed under load. Each test file starts a cluster of its own (three
# tenant nodes and fenec-shard over a new directory) and the app in its
# process. SAAS_BENCH=1 adds the measurements.
#
#   FENEC_SERVER, FENEC_SHARD   the binaries (this repository's target/release, then target/debug)
set -eu
cd "$(dirname "$0")/.."
npm run -s lint
npm test
if [ -n "${SAAS_BENCH:-}" ]; then npm run -s bench; fi
