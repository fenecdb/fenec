# fenecdb

# The wasm32 target comes with rustup. Homebrew's cargo has no wasm32 std
# library, so ~/.cargo/bin is preferred when it is present.
CARGO ?= $(shell test -x $(HOME)/.cargo/bin/cargo && echo $(HOME)/.cargo/bin/cargo || echo cargo)
PORT ?= 8787
SITE_PORT ?= 8788
WASM_OUT = target/wasm32-unknown-unknown/wasm/fenec_wasm.wasm
# The indexes the browser module is built with: every one unless named --
# `make wasm FEATURES="text sorted"`, or FEATURES=none for none of them.
FEATURES ?=
WASM_FEATURES = $(if $(FEATURES),--no-default-features $(if $(filter none,$(FEATURES)),,--features "$(FEATURES)"),)

.PHONY: all test test-js types types-check wasm wasm-lite wasm-sizes wasm-speed size-report packages version statements-bench file-bench web serve server node shard shard-bench replica-bench tx-bench concurrency-bench requests-bench load-bench maintenance-bench open-bench reopen-bench quant-bench scale-bench mirror-bench small bench sweep collate-bench \
	python-test drivers-test react-test langchain-test ai-sdk-test cloudflare-test cloudflare-bench \
	compare beir import-test follow-bench \
	pgvector-up pgvector-down docker docker-run docker-compact docker-down memory clean \
	site site-serve site-deploy

all: test wasm

## Rust first: `cargo test` also builds the `fenec-server` binary, and the sync
## tests on the JS side run against it (they skip themselves without it).
## Then fenec-core made without its indexes, as a small browser module is.
## fenec-bench has no tests, and SQLite's C source and the postgres client
## under it are the heaviest thing a build compiles; the examples are
## measurement programs, a fifth of what `cargo test` compiled. Clippy's
## --all-targets checks both, so neither rots.
test:
	$(CARGO) test --workspace --exclude fenec-bench --lib --bins --tests
	$(CARGO) test --workspace --exclude fenec-bench --doc
	$(CARGO) test -p fenec-core --no-default-features --features std-fs --lib --test features
	@$(MAKE) --no-print-directory test-js

## JS tests. node's own runner; no dependencies.
##   fenec.test.js       query builder (end-to-end too when wasm is present)
##   fenec.sync.test.js  sync layer -- against a real `fenec-server --http` server;
##                     skipped when `web/fenec.wasm` or the binary is missing
##   fenec.persist.test.js  incremental persistence, over an in-memory IndexedDB
##   fenec.file.test.js  a database kept in an OPFS file, over in-memory files,
##                     and handed to and from a real `fenec-server`
test-js:
	@if command -v node >/dev/null 2>&1; then \
		node --test web/fenec.test.js web/fenec.sync.test.js web/fenec.persist.test.js web/fenec.file.test.js; \
	else \
		echo "node not found -- JS tests skipped"; \
	fi

## web/fenec.d.ts held to web/fenec.js -- every export and method declared --
## and to what a caller writes, under tsc --strict
types-check:
	cd web/types && npm ci --no-audit --no-fund --loglevel=error && npm test

## TypeScript declarations from the schema: make types FILE=data.fenec
types:
	@test -n "$(FILE)" || (echo "usage: make types FILE=data.fenec"; exit 1)
	@$(CARGO) run -q --release -p fenec-cli -- types $(FILE) -o web/fenec-schema.d.ts

## Builds WASM for the browser and copies it under web/
#
# No wasm-opt step, and that is measured rather than assumed. On this module
# `-Oz` (binaryen 132) takes the raw file from 520 396 to 443 345 bytes, but
# the bytes it removes are ones the compressor was already removing -- both
# served sizes come out *worse*: gzip 186 401 -> 188 737, brotli 153 937 ->
# 156 820, and `-Os`, `-O2` and `-O3` within 0.3 KB of that. What is served
# is compressed, so the step costs ~2 900 bytes on every load to buy back
# 0.1 ms of a cold `WebAssembly.compile` (1.04 ms against 0.94 in Node 26's
# V8). Any network at all makes that a losing trade. It was the same trade
# at 334 KB, before the text index. The size comes from `[profile.wasm]` in
# Cargo.toml instead: opt-level "z", LTO, one codegen unit, panics that
# abort, symbols stripped -- and the link's `--compress-relocations`
# (.cargo/config.toml).
#
# Pass by pass at 441 KB, most of what any pass took was binaryen writing
# the module back: every one, a pass with nothing to do included, took 36.8
# KB and 2.3 KB brotli, which were the linker's padded LEBs, and which the
# link now leaves out itself. Beyond that only `--duplicate-function-
# elimination` took 0.4 KB brotli and a handful of passes under 0.1;
# `--merge-similar-functions`, `--reorder-functions` and `--local-cse` gave
# some of it back, and `--inlining` added 16.6 KB. opt-level "s" is 20%
# faster (`make wasm-speed`) for 16.4 KB more brotli, 12%.
wasm:
	@$(CARGO) build -p fenec-wasm --target wasm32-unknown-unknown --profile wasm $(WASM_FEATURES) 2>&1 | tail -2 || \
		(echo "the wasm32 target may be missing: rustup target add wasm32-unknown-unknown"; exit 1)
	@cp $(WASM_OUT) web/fenec.wasm
	@mkdir -p web/collate && rm -f web/collate/*.bin && cp crates/fenec-core/src/collate/*.bin web/collate/
	@echo "web/fenec.wasm  $$(wc -c < web/fenec.wasm) bytes"

## The module made without an index (FEATURES=none), for web/fenec.test.js
## to hand files between it and the full one.
wasm-lite:
	@$(CARGO) build -p fenec-wasm --target wasm32-unknown-unknown --profile wasm --no-default-features 2>&1 | tail -2
	@cp $(WASM_OUT) web/fenec-lite.wasm
	@echo "web/fenec-lite.wasm  $$(wc -c < web/fenec-lite.wasm) bytes"

## What counting a statement by its shape costs (/_stats/statements,
## pg_stat_statements): a million each, by one thread and by eight.
statements-bench:
	$(CARGO) run --release -p fenec-http --example statements

## The browser module's size built with each set of the four indexes
## (vector, text, sparse, sorted): raw, gzip -9 and brotli -q 11, in KB.
wasm-sizes:
	@python3 crates/fenec-wasm/sizes.py

## How fast the browser module is, in Node: an HNSW build, `near`, a filter
## and an order, `match`, a page of vectors as JSON. A list of .wasm files
## holds builds of it side by side: node crates/fenec-wasm/speed.mjs a.wasm b.wasm
wasm-speed: wasm
	@node crates/fenec-wasm/speed.mjs

## Where the browser module's bytes go: raw, gzip and brotli, its code by
## crate, module and part of the standard library, the largest functions and
## the generics whose copies weigh most. BASE=<git ref> builds that commit in
## a worktree beside it and shows what changed: make size-report BASE=main.
## WHY=<pattern> names the first of fenec's functions on each way to the
## functions matching it: make size-report WHY=flt2dec. BIN=fenec|fenec-server|
## fenec-shard reports that native binary's code by crate instead.
size-report:
	@python3 crates/fenec-wasm/size_report.py $(BASE) $(if $(WHY),--why '$(WHY)') $(if $(BIN),--bin $(BIN))

## PyPI's fenecdb and npm's @fenecdb/web and @fenecdb/react as a release
## publishes them, installed into a project and a venv of their own and
## used (PYTHON=... picks the interpreter; it wants 3.10 or newer).
packages: wasm wasm-lite
	@integrations/packages.sh

## One version wherever a release reads it: make version V=0.1.5
version:
	@python3 tools/version.py $(or $(V),$(error V=<version> is required))

## What keeping a database costs a page: IndexedDB (persist) against an
## OPFS file (openFile), 32 MB, in a worker of headless Chrome --
## BROWSER=safari opens Safari instead, BROWSER=firefox runs Firefox.
file-bench: wasm
	@python3 crates/fenec-bench/browser/serve.py

## Serves the browser demo locally
serve: wasm
	@echo ""
	@echo "  ->  http://localhost:$(PORT)"
	@echo ""
	@cd web && python3 -m http.server $(PORT)

## The server over ./data.fenec, HTTP on 127.0.0.1:8080 unless HTTP=addr
## names another (a token: make server TOKEN=secret)
server:
	$(CARGO) run --release -p fenec-server -- --http $(or $(HTTP),127.0.0.1:8080) --file data.fenec \
	  --sync 250 $(if $(TOKEN),--http-token $(TOKEN),)

## A tenant node: one file per tenant under ./tenants, HTTP only.
## make node HTTP=127.0.0.1:8081 ADMIN=secret
node:
	$(CARGO) run --release -p fenec-server -- --dir tenants --http $(or $(HTTP),127.0.0.1:8081) \
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
	$(CARGO) build --release -p fenec-server
	$(CARGO) run --release -p fenec-http --example replica -- 100000 128
	$(CARGO) run --release -p fenec-server --example failover -- 10

## A pg transaction: a lone write under each sync policy, a write in a
## transaction of 100, each in a savepoint, a ROLLBACK TO over 100, and a
## read, over the wire.
tx-bench:
	$(CARGO) run --release -p fenec-server --example transactions -- 20000

## Writers and readers at once, fenecdb against SQLite in one process:
## durable and buffered writes from 1, 4 and 16 threads, and reads alone,
## beside writers and beside a transaction held open.
concurrency-bench:
	$(CARGO) run --release -p fenec-bench --bin concurrency

## What one request costs over the wire: a row by id, a filter, a near and a
## put, over the pg wire's extended and simple protocols and over HTTP, one
## client at a time and eight at once, against fenec-server started here and
## PostgreSQL + pgvector (`make pgvector-up` first, skipped without it).
requests-bench:
	$(CARGO) build --release -p fenec-server
	$(CARGO) run --release -p fenec-bench --bin requests

## What loading 100 000 rows costs each way a client can send them: in
## process, the pg wire's simple and extended protocols, HTTP's array and
## /batch, with the vector index kept and without; PostgreSQL's COPY and
## INSERT beside them (`make pgvector-up` first, skipped without it).
load-bench:
	$(CARGO) build --release -p fenec-server
	$(CARGO) run --release -p fenec-bench --bin load

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
## crates/fenec-bench/beir/embed.mjs writes beside it (npm install there once);
## without them BM25 alone is scored. FENECBENCH_TEXT=chars (or prefix=6)
## gives the text index its options.
beir:
	@test -n "$(BEIR)" || (echo "usage: make beir BEIR=<dataset dir> (vectors: crates/fenec-bench/beir/embed.mjs; SPLADE, optional: splade.mjs)"; exit 1)
	$(CARGO) run --release -p fenec-bench --bin beir -- $(BEIR)

## The LangChain and LlamaIndex vector stores against their frameworks' own
## tests: fenec-server built and started here, the tests from a python:3.13
## container (Docker). The integrations may use outside packages; the
## crates may not.
python-test:
	integrations/python/run-tests.sh

## PostgreSQL drivers over the pg wire, pgvector's library for each: psycopg,
## asyncpg and SQLAlchemy from a python:3.13 container (Docker), pgx,
## node-postgres and tokio-postgres with the Go, Node and Rust on the machine
drivers-test:
	integrations/drivers/run-tests.sh

## useLiveQuery for React, against a stand-in and a real fenec-server + replica
## (needs `make wasm`)
react-test:
	@$(CARGO) build -q -p fenec-server
	cd integrations/react && npm ci --no-audit --no-fund --loglevel=error && npm test

## The LangChain.js vector store over a database in the page and over
## fenec-server's HTTP endpoint (needs `make wasm`)
langchain-test:
	@$(CARGO) build -q -p fenec-server
	cd integrations/langchain && npm ci --no-audit --no-fund --loglevel=error && npm test

## Retrieval for the Vercel AI SDK against its own mock models (needs `make wasm`)
ai-sdk-test:
	cd integrations/ai-sdk && npm ci --no-audit --no-fund --loglevel=error && npm test

## A database kept in a Durable Object's storage: persist, restore, and
## storage that fails part way, against a stand-in; then the example Worker
## under `wrangler dev`, stopped and started again (needs `make wasm`)
cloudflare-test:
	cd integrations/cloudflare && npm ci --no-audit --no-fund --loglevel=error && npm test

## A Durable Object's first answer after an eviction: the example Worker
## under `wrangler dev`, loaded, restarted, and its first `near` timed
## against the ones after it (needs `make wasm`; SIZES=1000,10000 to choose)
cloudflare-bench:
	cd integrations/cloudflare && npm ci --no-audit --no-fund --loglevel=error && node coldstart.mjs

## What `order ... collate tr` costs over a million Turkish names, in fenecdb
## and (after `make pgvector-up`) in PostgreSQL under ICU's tr-x-icu
collate-bench:
	$(CARGO) run --release -p fenec-bench --bin collate

## Verifies the import's PostgreSQL arm against a live server
import-test: pgvector-up
	@$(CARGO) test -p fenec-import --test all -- --ignored pg:: follow:: && \
	  $(CARGO) test -p fenec-server --test all -- --ignored follow::; \
	  status=$$?; $(MAKE) pgvector-down; exit $$status

## `fenec import --follow` against a live server: commit-to-visible latency,
## how fast a burst drains, how long a cut stream takes to come back.
## Needs `make pgvector-up` first.
follow-bench:
	$(CARGO) run --release -p fenec-import --example follow -- 10000 384

## Starts PostgreSQL with pgvector for the comparison. `wal_level=logical`
## is for `fenec import --follow` and its tests; it changes what is logged
## for updates and deletes, not how the compared reads run.
## `fenec-server --follow` serving what it follows: commit to a subscriber of
## the collection, and a server killed while the table is written to,
## started again. Needs `make pgvector-up` first.
mirror-bench:
	$(CARGO) build --release -p fenec-server
	$(CARGO) run --release -p fenec-server --example mirror -- 10000

## --shm-size: a parallel HNSW build holds its graph in dynamic shared
## memory, up to maintenance_work_mem, which scale-bench sets to 1 800 MB so
## that its graphs build in memory
pgvector-up:
	docker run -d --name fenecbench-pg --rm \
	  -e POSTGRES_PASSWORD=fenec -e POSTGRES_DB=fenecbench \
	  -p 55432:5432 --shm-size=2g pgvector/pgvector:pg17 \
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

## Starts the container: make docker-run TOKEN=secret
## The memory limit must be at least 3x the data file (compact peak).
docker-run: docker
	@test -n "$(TOKEN)" || (echo "token required: make docker-run TOKEN=secret"; exit 1)
	docker run -d --name fenecdb --restart unless-stopped \
	  -p 127.0.0.1:8080:8080 -v fenecdata:/data \
	  -e FENEC_HTTP_TOKEN=$(TOKEN) --memory 1g --memory-swap 1g fenecdb
	@echo "http://127.0.0.1:8080"

## Compacts the running container's file: what deletes and rewrites left
## dead goes, and every graph holding tombstones is built again.
docker-compact:
	@test -n "$(TOKEN)" || (echo "token required: make docker-compact TOKEN=secret"; exit 1)
	curl -fsS -H 'Authorization: Bearer $(TOKEN)' -d '{"query": "compact"}' \
	  http://127.0.0.1:8080/query

docker-down:
	-docker rm -f fenecdb

## What `create index` and `compact` cost the readers and writers of a
## running database: under the write lock, then beside it.
maintenance-bench:
	$(CARGO) run --release -p fenec-core --example maintenance -- 100000 128

## What opening a file costs, read into memory or mapped: a 1 GB file of
## 2.3 million rows, written once, then opened each way in a process of its
## own. OPEN_ROWS=23000000 makes it 10 GB.
OPEN_ROWS ?= 2300000
OPEN_FILE ?= target/open-$(OPEN_ROWS).fenec
open-bench:
	$(CARGO) build --release -p fenec-core --example open
	test -f $(OPEN_FILE) || ./target/release/examples/open write $(OPEN_FILE) $(OPEN_ROWS) 400 hs
	./target/release/examples/open open $(OPEN_FILE) read
	./target/release/examples/open open $(OPEN_FILE) mapped

## What a crash costs the next open: 100 000 x 768 written and never
## checkpointed, so every vector is in the tail, then opened as a server did
## (linked first) and does (linked beside the queries), alone and with two
## clients sending a near every 20 ms. Then written as a server keeps its
## graphs in the file, and crashed a row before the next one was due: the
## most a crash can leave to link. Each open is a process of its own.
REOPEN_ROWS ?= 100000
REOPEN_FILE ?= target/reopen-$(REOPEN_ROWS).fenec
REOPEN_KEPT ?= target/reopen-$(REOPEN_ROWS)-kept.fenec
reopen-bench:
	$(CARGO) build --release -p fenec-core --example reopen
	test -f $(REOPEN_FILE) || ./target/release/examples/reopen write $(REOPEN_FILE) $(REOPEN_ROWS) 768
	./target/release/examples/reopen open $(REOPEN_FILE) linked
	./target/release/examples/reopen open $(REOPEN_FILE) deferred
	./target/release/examples/reopen open $(REOPEN_FILE) deferred 2 20
	test -f $(REOPEN_KEPT) || ./target/release/examples/reopen write $(REOPEN_KEPT) $(REOPEN_ROWS) 768 worst
	./target/release/examples/reopen open $(REOPEN_KEPT) deferred

## Quantized vector indexes against full vectors: the arena, the heap,
## recall@10, latency and the documents' vectors read at beams of 100, 200
## and 400, and over a filtered set of 3 000 rows. QUANT_ROWS=1000000 is the
## million the docs quote; each mode runs in a process of its own.
QUANT_ROWS ?= 100000
quant-bench:
	$(CARGO) build --release -p fenec-core --example quant
	for m in none int8 bit; do ./target/release/examples/quant $(QUANT_ROWS) 768 $$m --rank 32 --filter 3000; done

## fenec-server against PostgreSQL + pgvector at scale, both over the pg wire
## (make pgvector-up first): the load and the index, memory, disk, recall@10
## and latency at beams of 40, 100 and 200, and eight clients' throughput.
## SCALE_ROWS x SCALE_DIM, a million 128-dim vectors unless given;
## SCALE_ARGS=--after builds fenec-server's index once the rows are in.
SCALE_ROWS ?= 1000000
SCALE_DIM ?= 128
scale-bench:
	$(CARGO) build --release -p fenec-server -p fenec-bench --bin fenec-server --bin scale
	./target/release/scale $(SCALE_ROWS) $(SCALE_DIM) $(SCALE_ARGS)

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
	rm -f web/fenec.wasm web/fenec-lite.wasm
	rm -rf web/collate site/dist
