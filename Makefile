# fenecdb

# The wasm32 target comes with rustup. Homebrew's cargo has no wasm32 std
# library, so ~/.cargo/bin is preferred when it is present.
CARGO ?= $(shell test -x $(HOME)/.cargo/bin/cargo && echo $(HOME)/.cargo/bin/cargo || echo cargo)
PORT ?= 8787
SITE_PORT ?= 8788
WASM_OUT = target/wasm32-unknown-unknown/wasm/fenec_wasm.wasm
# The indexes the browser module is built with: every one unless named --
# `make wasm FEATURES="text sorted"`, or FEATURES=none for none of them. The
# check a schema declared in code opens with comes with any set, unless
# SCHEMA=0: 7.5 KB brotli for a page whose code declares none.
FEATURES ?=
SCHEMA ?= 1
WASM_FEATURES = $(if $(FEATURES)$(filter 0,$(SCHEMA)),--no-default-features --features "$(if $(filter none,$(FEATURES)),,$(if $(FEATURES),$(FEATURES),indexes)) $(if $(filter 0,$(SCHEMA)),,schema)",)

.PHONY: all test test-js sync-scenarios-check builder-golden schema-golden types types-check docs-types ffi ffi-bench sync-bench swift-test kotlin-test dart-test wasm wasm-lite wasm-sizes wasm-speed wasm-exact-speed size-report packages version agents-md statements-bench file-bench web serve server node shard shard-bench replica-bench concurrency-bench requests-bench roundtrip-bench load-bench maintenance-bench compact-bench open-bench reopen-bench quant-bench scale-bench ycsb mirror-bench counters-bench small bench sweep collate-bench subquery-bench ttl-bench search-bench analytics-bench \
	python-test go-test dotnet-test languages-test examples-test react-test langchain-test ai-sdk-test cloudflare-test cloudflare-bench \
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
## The sync scenarios' reports go first, so a runner that did not run this
## time leaves none for sync-scenarios-check to find.
test:
	@rm -rf target/sync-scenarios
	$(CARGO) test --workspace --exclude fenec-bench --lib --bins --tests
	$(CARGO) test --workspace --exclude fenec-bench --doc
	$(CARGO) test -p fenec-core --no-default-features --features std-fs --lib --test features
	@$(MAKE) --no-print-directory test-js
	@if [ -f web/fenec.wasm ] && command -v node >/dev/null 2>&1; then $(MAKE) --no-print-directory sync-scenarios-check; fi

## JS tests. node's own runner; no dependencies. The spec reporter ends a
## failing run with each failing test's name and assertion: Node 22's
## default away from a terminal is TAP, a failure among a thousand lines.
##   fenec.test.js       query builder (end-to-end too when wasm is present)
##   fenec.sync.test.js  sync layer -- against a real `fenec-server --http` server;
##                     skipped when `web/fenec.wasm` or the binary is missing
##   fenec.sync.scenarios.test.js  integrations/sync-scenarios.json against
##                     FenecSync, over a scripted transport
##   fenec.persist.test.js  incremental persistence, over an in-memory IndexedDB
##   fenec.file.test.js  a database kept in an OPFS file, over in-memory files,
##                     and handed to and from a real `fenec-server`
##   fenec.schema.test.js  a schema declared in code (web/schema.js): what it
##                     declares, the check at an open, migrations, fenec types --schema
##   fenec.client.test.js  @fenecdb/web/client: the modules it reaches hold no
##                     engine, and its live queries over a scripted server
test-js:
	@if command -v node >/dev/null 2>&1; then \
		node --test --test-reporter=spec web/fenec.test.js web/fenec.sync.test.js web/fenec.sync.scenarios.test.js web/fenec.persist.test.js web/fenec.file.test.js web/fenec.schema.test.js web/fenec.client.test.js; \
	else \
		echo "node not found -- JS tests skipped"; \
	fi

## Both runners of integrations/sync-scenarios.json -- the native core's
## (fenec-abi's tests/scenarios.rs) and FenecSync's
## (web/fenec.sync.scenarios.test.js) -- passed every scenario: each writes
## what passed to target/sync-scenarios/, and a skip fails here
sync-scenarios-check:
	node integrations/sync-scenarios-check.mjs

## integrations/builder-golden.json written again from the JS builder's
## answers: the text and parameters the Python, Go and .NET builders are
## held to (web/golden.mjs holds the chains; fenec.test.js checks the file)
builder-golden:
	node web/golden.mjs

## AGENTS.md written again from CLAUDE.md: the same text under its own
## title (tools/agents_md.py; CI runs it with --check)
agents-md:
	python3 tools/agents_md.py

## integrations/schema-golden.json written again: each declaration's
## description, from web/schema.js, and each plan, from the engine (the
## module: make wasm first). crates/fenec-abi runs every plan case natively.
schema-golden:
	node web/schema-golden.mjs

## web/fenec.d.ts held to web/fenec.js -- every export and method declared --
## and to what a caller writes, under tsc --strict; then the docs' examples
types-check:
	cd web/types && npm ci --no-audit --no-fund --loglevel=error && npm test

## Every TypeScript example in the docs (site/content, data-lang="ts")
## type-checked under tsc --strict against the client's declarations
docs-types:
	cd web/types && npm ci --no-audit --no-fund --loglevel=error && npm run docs

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

## The module made without an index or the schema check (as `make wasm
## FEATURES=none SCHEMA=0`), for web/fenec.test.js to hand files between it
## and the full one, both ways. A test build: no package or release ships
## it, since a page either runs its queries on a server (@fenecdb/web/client)
## or takes the full module.
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

## `near` in the build without the indexes, which has no graph and measures every
## vector: 1 000, 10 000 and 50 000 rows of 128 and 384 dimensions, against
## the full module's walk and its own exact scan (ROWS=10000 for one size)
wasm-exact-speed: wasm wasm-lite
	@node crates/fenec-wasm/exact.mjs

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
## used (PYTHON=... picks the interpreter; it wants 3.10 or newer), and
## NuGet's FenecDb where the .NET SDK is installed.
packages: wasm
	@integrations/packages.sh
	@if command -v dotnet >/dev/null 2>&1; then integrations/dotnet/package.sh; \
	else echo "dotnet not found: FenecDb (NuGet) not checked"; fi

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

## Writers and readers at once, fenecdb against SQLite in one process:
## durable and buffered writes from 1, 4 and 16 threads, and reads alone,
## beside writers and beside blocks of 1 000 writes (a /batch's).
concurrency-bench:
	$(CARGO) run --release -p fenec-bench --bin concurrency

## A `set` of a constant against one of `n + 1`, over 100 000 rows and
## one row by id; 16 threads and 16 HTTP clients each incrementing one key
## 10 000 times, which must end at 160 000 (`set c {n: n + 1}`).
counters-bench:
	$(CARGO) build --release -p fenec-server
	$(CARGO) run --release -p fenec-bench --bin counters

## What one request costs over the wire: a row by id, a filter, a near and a
## put, over fenec-server's HTTP (POST /query, kept alive) and PostgreSQL's
## extended and simple protocols, one client at a time and eight at once,
## against fenec-server started here and PostgreSQL + pgvector (`make
## pgvector-up` first, skipped without it).
requests-bench:
	$(CARGO) build --release -p fenec-server
	$(CARGO) run --release -p fenec-bench --bin requests

## One client's round trip taken apart (crates/fenec-bench/src/bin/roundtrip.rs):
## YCSB's read by key and update of a field against fenec-server natively and
## in Docker -- built with `--features timing`, whose GET /_timing has each
## phase inside it -- and PostgreSQL 17 in Docker, its bind and execute
## logged. RT_ARGS: --systems server,server-docker,pg --modes always,250
## --records 100000 --ops 20000 --strace (the servers' system calls an op)
## --measure read,update,scan50 (scanN: N records from a key, with the bytes,
## packets and page faults of the container an op).
roundtrip-bench:
	$(CARGO) build --release -p fenec-server --features timing --target-dir target/timing
	$(CARGO) build --release -p fenec-bench --bin roundtrip
	docker build --build-arg FEATURES=timing -t fenecdb-timing .
	printf 'FROM alpine:3\nRUN apk add --no-cache strace\n' | docker build -t fenec-strace -
	./target/release/roundtrip $(RT_ARGS)

## What loading 100 000 rows costs each way a client can send them: in
## process, and over HTTP as a REST array, a /batch and a /query put of
## 1 000, with the vector index kept and without, then read back a page at
## a time; PostgreSQL's COPY and INSERT beside them (`make pgvector-up`
## first, skipped without it).
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

## The native library an app links (crates/fenec-ffi), in the ffi profile:
## for this machine, or TARGET=aarch64-linux-android and the like (the
## toolchain's linker for it on the PATH, the NDK's for Android). The size
## printed is the shared library's, stripped as the profile strips it --
## built alone, since beside the static library and the rlib its link-time
## optimisation left it 4% larger. JNI=1 adds the Kotlin binding's functions.
FFI_TARGET = $(or $(TARGET),$(shell rustc -vV | sed -n 's/^host: //p'))
ffi:
	@$(CARGO) rustc -q -p fenec-ffi --lib --profile ffi --target $(FFI_TARGET) $(if $(JNI),--features jni) --crate-type cdylib
	@for f in target/$(FFI_TARGET)/ffi/libfenec_ffi.dylib target/$(FFI_TARGET)/ffi/libfenec_ffi.so; do \
		test -f $$f && echo "$$f  $$(wc -c < $$f) bytes"; done; true

## A call through the native library against the same work through
## fenec-server's HTTP handler in process: an open, a put, a near
ffi-bench:
	$(CARGO) run --release -p fenec-ffi --example ffi_bench

## What the sync core (fenec_abi::sync) costs a change it applies, against
## the same rows put straight into the engine: a text row, a 128-dim row
## under HNSW, changes of one and of 100, a seed of 10 000
sync-bench:
	$(CARGO) run --release -p fenec-abi --example sync_bench

## The Swift package (Package.swift, integrations/swift) on macOS: the
## XCFramework's macOS slice for this machine, then swift test -- the
## engine, the builder over every golden case, live queries, a replica
## against a fenec-server the tests start -- and the tests again with
## Swift's cooperative pool cut to one thread (test-strict.sh)
swift-test:
	integrations/swift/run-tests.sh

## The Kotlin library's JVM tests (integrations/kotlin): the native library
## built for Linux with its JNI functions and the server the sync tests
## start, then JUnit under Gradle -- in rust and gradle:8-jdk17 containers
## unless this is Linux with Gradle
kotlin-test:
	integrations/kotlin/run-tests.sh

## The Dart package's tests (integrations/dart): the native library for
## this machine and the server the sync tests start, then dart test
## against them -- and flutter test for the plugin where Flutter is
## installed
dart-test:
	integrations/dart/run-tests.sh

## The Go SDK (integrations/go) against fenec-server processes its tests
## start: a primary, a replica of it and a node of tenants
go-test:
	@$(CARGO) build -q -p fenec-server
	cd integrations/go && go vet ./... && go test -count=1 ./...

## The .NET SDK (integrations/dotnet) the same way, with the .NET on the
## machine or, on Linux without one, from the dotnet/sdk:8.0 image
dotnet-test:
	integrations/dotnet/run-tests.sh

## The docs' example for each language over HTTP, against a real
## fenec-server: Python (the fenecdb client), Java, PHP and Ruby from
## containers of their own (Docker), JavaScript, Go, C# and Rust with the
## Node, Go, .NET and Rust on the machine
languages-test:
	integrations/languages/run-tests.sh

## The Notes examples (examples/) on this checkout's build, each its smoke
## test: Node, Python, Go, .NET, Rust and the Swift CLI with the toolchains
## on the machine, Kotlin's JVM CLI in Docker unless this is Linux with
## Gradle, Flutter (or its logic under plain Dart) where installed.
## E=python go for some of them
examples-test:
	examples/run-tests.sh $(E)

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

## A file under updates, compacted on its own or not: YCSB's 1 KB records,
## a field updated at a time while a thread reads whole records, the file's
## size, the reads' latency during and after each compact and how long its
## swap held the write lock, a line a second. COMPACT_ARGS, e.g.
## "--updates 5000000 --mode durable --writers 16 --auto off".
COMPACT_ARGS ?= --records 1000000 --updates 5000000
compact-bench:
	$(CARGO) build --release -p fenec-core --example compaction
	./target/release/examples/compaction $(COMPACT_ARGS)

## What `create index` and `compact` cost the readers and writers of a
## running database: under the write lock, then beside it.
maintenance-bench:
	$(CARGO) run --release -p fenec-core --example maintenance -- 100000 128

## `highlight()`, `snippet()` and `facet` over 100 000 documents: a row's
## marks against `match` alone, a facet by the buckets and by the scan, and
## the text index's build and `match` -- which must not move -- against a
## build from before them (the program runs there too, the rest `n/a`).
search-bench:
	$(CARGO) run --release -p fenec-core --example search -- 100000

## `in (get ...)` against the same `in [..]` written out, and against the
## `lookup ... required` that asks the same question from the other side:
## 20 000 customers and 200 000 orders.
subquery-bench:
	$(CARGO) run --release -p fenec-core --example subquery -- 20000

## Expressions, `group` by several keys and by `bucket`, `count(distinct)`,
## `first`/`last`: a million events and a million ticks, each question as one
## query against the queries a client sent before; then the fixed aggregates
## of before (`analytics old` asks only those, for another commit).
analytics-bench:
	$(CARGO) run --release -p fenec-core --example analytics

## `@ttl`: reads of a million rows half past their time against the same
## rows with no expiry and with the expiry written out by hand, then a
## sweep of 100 000 expired rows out of a file, the write lock held a batch.
ttl-bench:
	$(CARGO) build --release -p fenec-core --example expiry
	./target/release/examples/expiry reads 1000000
	./target/release/examples/expiry sweep 100000 1000

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

## fenec-server over HTTP against PostgreSQL + pgvector over its own wire, at
## scale (make pgvector-up first): the load and the index, memory, disk,
## recall@10 and latency at beams of 40, 100 and 200, and eight clients'
## throughput. SCALE_ROWS x SCALE_DIM, a million 128-dim vectors unless
## given; SCALE_ARGS=--after builds fenec-server's index once the rows are
## in, SCALE_ARGS="--only fenec" (or pg) runs one side, for running them in
## turns after idle minutes.
SCALE_ROWS ?= 1000000
SCALE_DIM ?= 128
scale-bench:
	$(CARGO) build --release -p fenec-server -p fenec-bench --bin fenec-server --bin scale
	./target/release/scale $(SCALE_ROWS) $(SCALE_DIM) $(SCALE_ARGS)

## YCSB's core workloads A-F (crates/fenec-bench/src/bin/ycsb.rs): fenecdb in
## process against SQLite, fenec-server over HTTP against PostgreSQL 17 and
## MongoDB 8, each in a container the bench starts and removes in its turn.
## 1 000 000 records, 1/4/16 threads, durable and buffered, 30 s a cell after
## a 5 s warm-up, each cell after the CPU has cooled; a line a cell into
## ycsb.tsv, `./target/release/ycsb report ycsb.tsv` the medians. Narrow it
## with YCSB_ARGS, e.g. "--records 100000 --systems fenec,sqlite --workloads
## AC --threads 1,16 --runs 3". --systems server-docker runs the server in a
## container too, from `docker build -t fenecdb-ycsb .`.
ycsb:
	$(CARGO) build --release -p fenec-server -p fenec-bench --bin fenec-server --bin ycsb
	./target/release/ycsb $(YCSB_ARGS)

## Memory footprint (for calibrating --max-memory)
memory:
	$(CARGO) run --release -p fenec-core --example memory -- 100000 128

## ef / recall trade-off
sweep:
	$(CARGO) run --release -p fenec-core --example sweep -- 50000 128

## The website and the documentation. Plain static files, stdlib-only
## generator; the live console on the home page needs `make wasm` first.
site: wasm wasm-lite
	python3 site/build.py

## Same, on http://localhost:8788
site-serve: wasm wasm-lite
	python3 site/build.py --serve --port $(SITE_PORT)

## Publishes to Cloudflare Workers (fenecdb.com). Needs `wrangler login`
## or CLOUDFLARE_API_TOKEN; CI does the same thing on a push to main.
site-deploy: site
	npx wrangler deploy

clean:
	$(CARGO) clean
	rm -f web/fenec.wasm web/fenec-lite.wasm
	rm -rf web/collate site/dist
