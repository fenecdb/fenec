#!/bin/sh
# YCSB with Redis beside fenec-server -- on the Mac and in Docker --,
# PostgreSQL and MongoDB, in turns, three runs: the lines
# benchmarks.html#ycsb-redis is written from once they are in
# results.tsv. Hours long (about nine on the M1 Air: 33 cells a system a
# run, Redis's 48 with its snapshot mode, each 45 s with its idle gap, the
# loads, three idle minutes between systems), so it runs in the background
# and appends as it goes:
#
#   nohup crates/fenec-bench/ycsb/run-redis.sh > ycsb-redis.log 2>&1 &
#
# from the repository's root, on a machine doing nothing else: the probe
# each cell waits on (`cool`) catches a CPU that is hot, not one another
# program is using, and the idle speed it holds them to is measured at the
# start. So the script first waits for the load average to fall under
# QUIET_LOAD (2 by default); PROBE_BASE=<ms> holds the cells to an idle
# probe measured before instead (62.5 ms on the Air).
#
# First a --verify pass over Redis, A to F at 1 and 16 clients, its three
# modes, into verify.tsv as run `rv`; the measurement starts only if it
# found no mismatch. Then the measurement, runs `rd1`-`rd3`, into
# results.tsv. Its cells of fenec-server, fenec-server in Docker,
# PostgreSQL and MongoDB join the medians of those cells' earlier runs:
# commit them with the pages `CI=1 python3 site/build.py` then flags, or
# name the earlier runs in superseded.tsv so every compared cell is of one
# day.
set -eu
cd "$(dirname "$0")/../../.."

results=crates/fenec-bench/ycsb/results.tsv
verify=crates/fenec-bench/ycsb/verify.tsv
probe=${PROBE_BASE:+--probe-base $PROBE_BASE}

# Kept awake: the Mac sleeping in the middle of a run cuts it short.
awake=""
if command -v caffeinate >/dev/null 2>&1; then
    awake="caffeinate -i"
fi

load() {
    if [ "$(uname)" = Darwin ]; then
        sysctl -n vm.loadavg | awk '{print $2}'
    else
        cut -d' ' -f1 /proc/loadavg
    fi
}
while [ "$(load | awk -v q="${QUIET_LOAD:-2}" '{print ($1 < q)}')" != 1 ]; do
    echo "$(date '+%H:%M') load $(load), waiting for a quiet machine"
    sleep 60
done

# fenec-server's image for `server-docker`, and the images the others run.
docker build -t fenecdb-ycsb .
for image in redis:7 postgres:17 mongo:8 alpine; do
    docker image inspect "$image" >/dev/null 2>&1 || docker pull "$image"
done

echo "$(date '+%F %H:%M') verify pass"
# shellcheck disable=SC2086
$awake make ycsb YCSB_ARGS="--systems redis --records 100000 --seconds 10 \
    --threads 1,16 --modes durable,buffered,snapshot --run-id rv \
    --verify $verify --out ${TMPDIR:-/tmp}/ycsb-redis-verify.tsv $probe"
wrong=$(awk -F'\t' '$1 == "rv" && $2 == "redis" && ($10 != 0 || $11 != 0)' "$verify")
if [ -n "$wrong" ]; then
    echo "--verify found answers not as written:"
    echo "$wrong"
    exit 1
fi

echo "$(date '+%F %H:%M') measurement"
# shellcheck disable=SC2086
$awake make ycsb YCSB_ARGS="--systems server,server-docker,pg,mongo,redis \
    --modes durable,buffered,snapshot --runs 3 --run-id rd --out $results $probe"
echo "$(date '+%F %H:%M') done: ./target/release/ycsb report $results"
