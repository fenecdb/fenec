#!/bin/zsh
# Official YCSB 0.17.0 against the same containers our harness measures.
set -u
# YCSB 0.17.0's jdbc and mongodb binding tarballs, a JRE 17 and PostgreSQL's
# JDBC driver 42.7.4 unpacked into $J; the harness built (make ycsb).
S=${S:-/tmp/ycsb-calibration}
W=${W:-$(git rev-parse --show-toplevel)}
J=${J:-$S/ycsb-java}
JAVA=$J/jdk-17.0.20.1+1-jre/Contents/Home/bin/java
OUT=$S/calib
mkdir -p $OUT
idle() { echo "$(date +%T) idle $1 s"; sleep $1; }
ycsb() { # binding-dir db-class workload threads phase extra...
  local dir=$1 db=$2 wl=$3 t=$4 phase=$5; shift 5
  $JAVA -cp "$J/$dir/lib/*:$J/postgresql.jar" site.ycsb.Client $phase -db $db -P $J/$dir/workloads/$wl \
    -p recordcount=100000 -p operationcount=1000000000 -p maxexecutiontime=30 -threads $t -s "$@"
}
for sys in pg mongo; do
  docker rm -f -v fenecycsb-$sys >/dev/null 2>&1; docker volume rm -f fenecycsb-$sys >/dev/null 2>&1
  if [[ $sys == pg ]]; then
    docker run -d --name fenecycsb-pg -e POSTGRES_PASSWORD=fenec -e POSTGRES_DB=ycsb -p 127.0.0.1:55433:5432 --shm-size=1g \
      -v fenecycsb-pg:/var/lib/postgresql/data postgres:17 -c shared_buffers=1GB -c effective_cache_size=2GB \
      -c max_wal_size=4GB -c max_connections=200 >/dev/null
    until docker exec fenecycsb-pg psql -U postgres -d ycsb -c 'select 1' >/dev/null 2>&1; do sleep 1; done
    export FENECBENCH_YCSB_PG="host=127.0.0.1 port=55433 user=postgres password=fenec dbname=ycsb"
    DIR=ycsb-jdbc-binding-0.17.0; DB=site.ycsb.db.JdbcDBClient
    PROPS=(-p db.driver=org.postgresql.Driver -p db.url=jdbc:postgresql://127.0.0.1:55433/ycsb -p db.user=postgres -p db.passwd=fenec)
  else
    docker run -d --name fenecycsb-mongo -p 127.0.0.1:27018:27017 -v fenecycsb-mongo:/data/db mongo:8 >/dev/null
    until docker exec fenecycsb-mongo mongosh --quiet --eval 'db.runCommand({ping:1})' >/dev/null 2>&1; do sleep 1; done
    export FENECBENCH_YCSB_MONGO="mongodb://127.0.0.1:27018/?maxPoolSize=64"
    DIR=ycsb-mongodb-binding-0.17.0; DB=site.ycsb.db.MongoDbClient
    PROPS=(-p 'mongodb.url=mongodb://127.0.0.1:27018/ycsb?w=1&journal=true')
  fi
  idle 120
  # Ours: the same cells, durable, 100 000 records.
  $W/target/release/ycsb --systems $sys --records 100000 --modes durable --workloads AC --threads 1,16 \
    --out $OUT/ours.tsv --run-id c > $OUT/ours-$sys.log 2>&1
  idle 120
  if [[ $sys == pg ]]; then
    docker exec fenecycsb-pg psql -U postgres -d ycsb -c 'DROP TABLE IF EXISTS usertable' \
      -c 'CREATE TABLE usertable (YCSB_KEY VARCHAR(255) PRIMARY KEY, FIELD0 TEXT, FIELD1 TEXT, FIELD2 TEXT, FIELD3 TEXT, FIELD4 TEXT, FIELD5 TEXT, FIELD6 TEXT, FIELD7 TEXT, FIELD8 TEXT, FIELD9 TEXT)'
  else
    docker exec fenecycsb-mongo mongosh --quiet ycsb --eval 'db.usertable.drop()'
  fi
  ycsb $DIR $DB workloada 8 -load $PROPS > $OUT/ycsb-$sys-load.txt 2>&1
  [[ $sys == pg ]] && docker exec fenecycsb-pg psql -U postgres -d ycsb -c 'VACUUM ANALYZE usertable' -c CHECKPOINT
  for wl in workloada workloadc; do
    for t in 1 16; do
      idle 60
      ycsb $DIR $DB $wl $t -t $PROPS > $OUT/ycsb-$sys-$wl-$t.txt 2>&1
      grep -E "OVERALL\], Throughput|99thPercentileLatency|FAILED|Return=ERROR" $OUT/ycsb-$sys-$wl-$t.txt | sed "s/^/$sys $wl $t /"
    done
  done
  unset FENECBENCH_YCSB_PG FENECBENCH_YCSB_MONGO
  docker rm -f -v fenecycsb-$sys >/dev/null; docker volume rm -f fenecycsb-$sys >/dev/null
  idle 120
done
echo "$(date +%T) calibration done"
