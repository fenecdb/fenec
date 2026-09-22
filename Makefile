# fenecdb

# The wasm32 target comes with rustup. Homebrew's cargo has no wasm32 std
# library, so ~/.cargo/bin is preferred when it is present.
CARGO ?= $(shell test -x $(HOME)/.cargo/bin/cargo && echo $(HOME)/.cargo/bin/cargo || echo cargo)
PORT ?= 8787
SITE_PORT ?= 8788
WASM_OUT = target/wasm32-unknown-unknown/wasm/fenec_wasm.wasm

.PHONY: all test test-js types wasm web serve pg node shard shard-bench replica-bench maintenance-bench small bench sweep \
	compare beir import-test follow-bench \
	pgvector-up pgvector-down docker docker-run docker-compact docker-down memory clean \
	site site-serve site-deploy

all: test wasm

## Rust first: `cargo test` also builds the `fenec-pg` binary, and the sync
## tests on the JS side run against it (they skip themselves without it).
test:
	$(CARGO) test
	@$(MAKE) --no-print-directory test-js

## JS tests. node's own runner; no dependencies.
##   fenec.test.js       query builder (end-to-end too when wasm is present)
##   fenec.sync.test.js  sync layer -- against a real `fenec-pg --http` server;
##                     skipped when `web/fenec.wasm` or the binary is missing
##   fenec.persist.test.js  incremental persistence, over an in-memory IndexedDB
test-js:
	@if command -v node >/dev/null 2>&1; then \
		node --test web/fenec.test.js web/fenec.sync.test.js web/fenec.persist.test.js; \
	else \
		echo "node not found -- JS tests skipped"; \
	fi

## TypeScript declarations from the schema: make types FILE=data.fenec
types:
	@test -n "$(FILE)" || (echo "usage: make types FILE=data.fenec"; exit 1)
	@$(CARGO) run -q --release -p fenec-cli -- types $(FILE) -o web/fenec-schema.d.ts

## Builds WASM for the browser and copies it under web/
#
# No wasm-opt step, and that is measured rather than assumed. On this module
# `-Oz` takes the raw file from 334 131 to 281 738 bytes, but the bytes it
# removes are ones the compressor was already removing -- both served sizes
# come out *worse*: gzip 116 911 -> 118 932, brotli 97 169 -> 98 970. What is
# served is compressed, so the step costs ~1 800 bytes on every load to buy
# back a fraction of a millisecond of `WebAssembly.compile` (0.9 ms against
# 0.8 ms, cold, in Chrome, measured before the text index went in). Any
# network at all makes that a losing trade.
wasm:
	@$(CARGO) build -p fenec-wasm --target wasm32-unknown-unknown --profile wasm 2>&1 | tail -2 || \
		(echo "the wasm32 target may be missing: rustup target add wasm32-unknown-unknown"; exit 1)
	@cp $(WASM_OUT) web/fenec.wasm
	@echo "web/fenec.wasm  $$(wc -c < web/fenec.wasm) bytes"

## Serves the browser demo locally
serve: wasm
	@echo ""
	@echo "  ->  http://localhost:$(PORT)"
	@echo ""
	@cd web && python3 -m http.server $(PORT)

## PostgreSQL protocol server (for a password: make pg PGPASS=secret)
## For the HTTP/JSON endpoint: make pg HTTP=127.0.0.1:8080
pg:
	$(CARGO) run --release -p fenec-pg -- --listen 127.0.0.1:5433 --file data.fenec \
	  --sync 250 $(if $(PGPASS),--password $(PGPASS),) $(if $(HTTP),--http $(HTTP),)

## A tenant node: one file per tenant under ./tenants, HTTP only.
## make node HTTP=127.0.0.1:8081 ADMIN=secret
node:
	$(CARGO) run --release -p fenec-pg -- --dir tenants --http $(or $(HTTP),127.0.0.1:8081) \
	  --admin-token $(or $(ADMIN),$(error ADMIN=<token> is required)) --sync 250

## The router in front of the nodes; the directory lives in ./shard.fenec.
shard:
	$(CARGO) run --release -p fenec-shard -- --listen 127.0.0.1:8090 --directory shard.fenec

## What the router adds per request, and how long a tenant move takes.
shard-bench:
	$(CARGO) run --release -p fenec-shard --example overhead -- 100000 128

## Replication: a replica's lag under each sync policy, how fast it
## catches up and starts from an image, and what a failover loses.
replica-bench:
	$(CARGO) build --release -p fenec-pg
	$(CARGO) run --release -p fenec-http --example replica -- 100000 128
	$(CARGO) run --release -p fenec-pg --example failover -- 10

## When size comes first: no import, abort instead of panic unwinding.
small:
	@$(CARGO) build --profile cli -p fenec-cli --no-default-features
	@echo "target/cli/fenec  $$(wc -c < target/cli/fenec) bytes"

## Scale measurement (clustered embedding distribution). --uniform gives
## the pathological case.
bench:
	$(CARGO) run --release -p fenec-core --example bench -- 100000 128

## Comparison against SQLite and PostgreSQL.
## For the PostgreSQL arm, first: make pgvector-up
compare:
	$(CARGO) run --release -p fenec-bench -- 100000 128

## Retrieval quality on BEIR, nDCG@10 for every way of ranking ten documents:
##   make beir BEIR=path/to/scifact
## The directory is one of BEIR's zips unpacked, with the vectors
## crates/fenec-bench/beir/embed.mjs writes beside it (npm install there once).
beir:
	@test -n "$(BEIR)" || (echo "usage: make beir BEIR=<dataset dir> (vectors: crates/fenec-bench/beir/embed.mjs)"; exit 1)
	$(CARGO) run --release -p fenec-bench --bin beir -- $(BEIR)

## Verifies the import's PostgreSQL arm against a live server
import-test: pgvector-up
	@$(CARGO) test -p fenec-import --test pg --test follow -- --ignored; \
	  status=$$?; $(MAKE) pgvector-down; exit $$status

## `fenec import --follow` against a live server: commit-to-visible latency,
## how fast a burst drains, how long a cut stream takes to come back.
## Needs `make pgvector-up` first.
follow-bench:
	$(CARGO) run --release -p fenec-import --example follow -- 10000 384

## Starts PostgreSQL with pgvector for the comparison. `wal_level=logical`
## is for `fenec import --follow` and its tests; it changes what is logged
## for updates and deletes, not how the compared reads run.
pgvector-up:
	docker run -d --name fenecbench-pg --rm \
	  -e POSTGRES_PASSWORD=fenec -e POSTGRES_DB=fenecbench \
	  -p 55432:5432 --shm-size=1g pgvector/pgvector:pg17 \
	  -c shared_buffers=1GB -c maintenance_work_mem=1GB \
	  -c max_parallel_workers_per_gather=0 -c wal_level=logical
	@echo "waiting for it to become ready..."
	@until docker exec fenecbench-pg pg_isready -U postgres -d fenecbench >/dev/null 2>&1; do sleep 1; done
	@echo "postgres://postgres:fenec@127.0.0.1:55432/fenecbench"

pgvector-down:
	-docker rm -f fenecbench-pg

## Builds the server image (static with musl, ~4 MB)
docker:
	docker build -t fenecdb .

## Starts the container: make docker-run PGPASS=secret
## The memory limit must be at least 3x the data file (compact peak).
docker-run: docker
	@test -n "$(PGPASS)" || (echo "password required: make docker-run PGPASS=secret"; exit 1)
	docker run -d --name fenecdb --restart unless-stopped \
	  -p 127.0.0.1:5433:5433 -v fenecdata:/data \
	  -e FENECPG_PASSWORD=$(PGPASS) --memory 1g --memory-swap 1g fenecdb
	@echo "postgres://fenec@127.0.0.1:5433"

## Writes the graph to the file. fenec-pg writes no checkpoint on shutdown:
## without this call every restart rebuilds the HNSW index from scratch.
docker-compact:
	@test -n "$(PGPASS)" || (echo "password required: make docker-compact PGPASS=secret"; exit 1)
	docker run --rm --network container:fenecdb -e PGPASSWORD=$(PGPASS) \
	  postgres:16-alpine psql -h 127.0.0.1 -p 5433 -U fenec -c 'compact'

docker-down:
	-docker rm -f fenecdb

## What `create index` and `compact` cost the readers and writers of a
## running database: under the write lock, then beside it.
maintenance-bench:
	$(CARGO) run --release -p fenec-core --example maintenance -- 100000 128

## Memory footprint (for calibrating --max-memory)
memory:
	$(CARGO) run --release -p fenec-core --example memory -- 100000 128

## ef / recall trade-off
sweep:
	$(CARGO) run --release -p fenec-core --example sweep -- 50000 128

## The website and the documentation. Plain static files, stdlib-only
## generator; the live console on the home page needs `make wasm` first.
site: wasm
	python3 site/build.py

## Same, on http://localhost:8788
site-serve: wasm
	python3 site/build.py --serve --port $(SITE_PORT)

## Publishes to Cloudflare Workers (fenecdb.com). Needs `wrangler login`
## or CLOUDFLARE_API_TOKEN; CI does the same thing on a push to main.
site-deploy: site
	npx wrangler deploy

clean:
	$(CARGO) clean
	rm -f web/fenec.wasm
	rm -rf site/dist
