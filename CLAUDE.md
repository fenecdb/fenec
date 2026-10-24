# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

fenecdb — a minimal embedded database in Rust. Compiles to WASM for
the browser, has its own query language (FenecQL), and its server (`fenec-server`)
speaks HTTP; every language reaches it that way, Python, JavaScript, Go,
.NET, Swift, Kotlin and Dart through official clients. The docs under `site/content/docs/` are the long-form reference (design
rationale, benchmarks, full FenecQL and HTTP surface); `README.md` is the
front door and links into them, and this file is the working summary.
Each invariant here is its rule; its long form -- measurements, rejected
alternatives, history -- is in `docs/design/`, whose `README.md` gives
their order, and `tools/agents_md.py --check` fails once this file passes
64 000 bytes.
`AGENTS.md` is this file for other agents, the same text under its own title
and first line: `make agents-md` writes it from `CLAUDE.md`
(`tools/agents_md.py`), and CI fails when the two differ, so edit `CLAUDE.md`.

## Commands

```bash
make test          # cargo test (no fenec-bench, no examples), fenec-core without its indexes and without any feature, then the JS tests and sync-scenarios-check
make wasm          # builds fenec-wasm for wasm32, copies to web/fenec.wasm
make wasm FEATURES="text sorted"   # with those features alone (indexes, language: a group; none: none)
make wasm-lite     # the test build without the indexes, to web/fenec-lite.wasm (web/fenec.test.js; shipped by nothing)
make wasm-min      # the test build with no feature at all, to web/fenec-min.wasm (the same)
make wasm-sizes    # what each feature adds to the module with none, and what leaving it out of the full one saves
make size-report   # where the module's bytes go, by crate, module and std (BASE=main: against main; BIN=fenec-server: a native binary's; FEATURES=none: that build's)
make wasm-speed    # the module in Node: HNSW build, near, filter, match, JSON (speed.mjs a.wasm b.wasm compares builds)
make wasm-exact-speed   # near without the graph (the wasm-lite build), 1k-50k rows x 128/384, against the graph and exact
make file-bench    # what keeping a database costs a page: IndexedDB (persist) against an OPFS file (openFile), 32 MB, headless Chrome (BROWSER=safari|firefox)
make ffi           # the native library for apps (crates/fenec-ffi) for this machine, TARGET=... another, JNI=1 with the Kotlin functions
make ffi-bench     # a call through the native library against fenec-server's handler in process: open, put, near
make sync-bench    # the sync core (fenec_abi::sync) a change applied, against the same put alone
make sync-scenarios-check   # both runners of integrations/sync-scenarios.json passed every scenario (after make test)
make swift-test    # the Swift package (Package.swift) on macOS: the XCFramework's macOS slice, swift test (sync tests start a fenec-server, read-routing tests fenec-shard routers too), then again on one cooperative thread
make kotlin-test   # the Kotlin library's JVM tests, the library built for Linux (Docker unless Linux with Gradle)
make dart-test     # the Dart package against the library for this machine, and the Flutter plugin where Flutter is installed
make packages      # fenecdb (PyPI), the @fenecdb npm packages and FenecDb (NuGet) as a release publishes them, installed and used
make version V=X.Y.Z   # one version wherever a release reads it (RELEASING.md)
make agents-md     # AGENTS.md written again from this file (CI fails when they differ)
make serve         # wasm + python3 http.server -> http://localhost:8787
make bench         # scale measurement (fenec-core/examples/bench.rs)
make memory        # memory footprint, for calibrating --max-memory
make sweep         # ef / recall trade-off
make compare       # vs SQLite + pgvector (needs `make pgvector-up` first)
make python-test   # LangChain + LlamaIndex stores vs their frameworks' tests (Docker)
make languages-test   # the docs' example in Python, JS, Go, C#, Java, PHP, Ruby and Rust over HTTP (Docker for some)
make examples-test    # examples/: the Notes app in every language, each its smoke on this checkout's build (E="python go" for some)
make go-test       # the Go SDK (integrations/go) against a primary, a replica and a tenant node its tests start
make dotnet-test   # the .NET SDK (integrations/dotnet) the same way, xunit; the dotnet/sdk:8.0 image on Linux without .NET
make builder-golden   # integrations/builder-golden.json written again from the JS builder (web/golden.mjs)
make schema-golden    # integrations/schema-golden.json: declarations, FenecQL texts and plans, through the module (web/schema-golden.mjs)
make types-check  # web/fenec.d.ts held to web/fenec.js and to a caller's code under tsc --strict, then make docs-types (web/types)
make docs-types   # every data-lang="ts" example on the site under tsc --strict (web/types/docs.mjs; part of make types-check)
make react-test    # useLiveQuery vs a real fenec-server replica (needs `make wasm`)
make langchain-test   # the LangChain.js vector store over a database in the page and over fenec-server (needs `make wasm`)
make ai-sdk-test   # integrations/ai-sdk/rag.js against the Vercel AI SDK's own mock models (needs `make wasm`)
make cloudflare-test  # a database in a Durable Object's storage, SQL rows and values: persist, restore, storage failing part way, the example Worker under wrangler dev; then the global entry (entry/) in front of two regions' routers and of stand-ins
make cloudflare-bench # a Durable Object's first near after an eviction, under wrangler dev, against the ones after (SIZES=1000,10000 to choose); rows a write and a checkpoint cost; a restore's peak memory
make entry-bench      # the global entry under wrangler dev: a read by id through it, its tenant's home heard, in the colo's cache and never heard (a relay), against the router asked directly, in turns
make mcp-test      # the MCP server vs the official TypeScript MCP client: HTTP and stdio, both eras, the docs' configs
make studio-test   # fenec studio (--studio) in headless Chrome: browse, edit, a scoped token, 100k rows
make studio-highlight   # studio/highlight.js and the Grafana data source's copy written again from site/highlight.js and build.py's rules
make grafana-check # monitoring/'s compose (Docker): both data dashboards, Infinity's and the Grafana plugin's (integrations/grafana), and its alert rule held to fenec-server
make deploy-check  # deploy/: promtool over the alert rules and their tests, the tunnel's rules asked of cloudflared, the images' build script (tools/image-cargo.sh) held to a tree older than its last build and to a unit compiled from other bytes, then every systemd unit in two privileged containers booting systemd -- verified, started, used, the archive copied by real rclone, a standby router promoted by the units
make drill         # deploy/'s failover drill (Docker): two regions, a node cut off and killed and back, a router killed and its standby promoted, the global routers cut off, a restore; lost must be 0 (DRILL_LEASE, DRILL_KEEP=1)
make beir BEIR=dir # nDCG@10 per ranking path (vectors: crates/fenec-bench/beir, embed.mjs + splade.mjs; BM25 alone without; FENECBENCH_TEXT=chars sets @text's options)
make import-test   # the PostgreSQL arm of import and --follow (needs Docker)
make follow-bench  # --follow: commit-to-visible latency, drain, reconnect (pgvector-up first)
make mirror-bench  # fenec-server --follow: commit to a subscriber, a server killed and started again (pgvector-up first)
make pgvector-up   # PostgreSQL 17 + pgvector in Docker on 127.0.0.1:55432, wal_level=logical, for the comparisons and --follow (pgvector-down removes it)
make small         # smallest `fenec` binary: --profile cli --no-default-features
make server TOKEN=secret HTTP=127.0.0.1:8080   # run the server against ./data.fenec
make node ADMIN=secret   # a tenant node: fenec-server --dir tenants
make shard               # the router in front of the nodes (./shard.fenec)
make shard-bench         # router overhead per request, tenant move time, failovers by hand and on a lease, a query across 100 tenants (ARGS=latency|fanout for one part; ARGS=standby a standby router's request against the primary's; ARGS=regions a region's own failover cut off from the global router, leases of 1 and 5 s; ARGS=reads a read answered by a read replica against one sent home)
make quota-bench         # a tenant's quotas: a put held to the limit in process, a tenant with every quota against one with none over HTTP
make fence-bench         # a node's lease: a put in process with no fence and with the lease's, and Lease::allows alone
make policy-bench        # a scoped set under a plain rule, a field list, a transition, answering its row and a rule reading another collection, and a read by id, against the server's token, in process and over HTTP (ARGS=streams|beats: writes beside scoped streams)
make replica-bench       # replica lag per sync policy, catch-up, what a failover loses
make node-archive-bench  # every tenant of a node archived: 1 000 tenants and one of 64 MB, a stream each against the node's one stream (ARGS="tenants active seconds sync way big_mb"); the syncer's passes and a close beside it (PASSES="way seconds big_mb")
make concurrency-bench   # writers and readers at once against SQLite: durable and buffered writes, reads beside blocks of writes (ARGS=blocks those alone, NOPREP=1 a block built under the lock; ARGS=long writers beside long reads)
make requests-bench      # a request over HTTP: one client's round trip, eight's rate, against PostgreSQL
make mcp-bench           # an MCP tool call against the /query it runs: in process, and over HTTP
make pages-bench         # pages of every size and place, limit and offset written in against as parameters: the parse cache's hits
make recon-bench         # a ledger's reconciliation /batch beside transfers and reads: their longest waits
make roundtrip-bench     # one client's round trip taken apart: the client, the server's phases (--features timing, GET /_timing), PostgreSQL's bind and execute
make load-bench          # loading 100 000 rows each way a client can send them, against PostgreSQL's COPY and INSERT
make maintenance-bench   # reads and writes during create index / compact
make compact-bench       # a file under updates, compacted on its own or not: its size, reads during and after each compact, its swap's lock (COMPACT_ARGS)
make open-bench          # opening a 1 GB file, read into memory or mapped
make opens-bench         # a checkpointed file opened, alone and with its first read: 1k and 100k x 128, a million rows of no vector
make reopen-bench        # a crashed 100k x 768 file: linked at the open, beside the queries, or with its graphs kept
make quant-bench         # quant=int8|bit against full vectors: memory, recall, latency
make scale-bench         # fenec-server over HTTP against pgvector at scale: load, memory, recall, latency, filters (pgvector-up first)
make ycsb                # YCSB A-F: fenecdb vs SQLite in process, fenec-server vs PostgreSQL, MongoDB and Redis in Docker, durable and buffered (YCSB_ARGS; ycsb/run-redis.sh the Redis runs)
make statements-bench    # what counting a statement by its shape costs
make subquery-bench      # in (get ...) against its list written out and against lookup ... required
make composite-bench     # an index over several fields: a user's latest twenty and a feed over ten through @sorted(owner, created desc) against @hash, @sorted and the scan; @unique(org, slug)
make lookup-bench        # a lookup over a key of several fields: a tenant's 20 shops x 3 orders x 5 lines against the key joined into one @hash field (ARGS="all 500 2", ARGS=single)
make search-bench        # highlight(), snippet() and facet over 100 000 documents: a row's marks, a facet by buckets and by scan, by ranges, disjunctive
make fuzzy-bench         # match ... fuzzy and prefix over 100 000 documents: the sorted words' build and size, the walk at one edit and two, a query plain, with a typo and half typed (BEIR=dir: its words too)
make text-memory         # what a text index holds, counted by the allocator: a tenant's notes over a Zipf vocabulary and of made-up words, musl's slots for them and memory_bytes (BEIR="a b": those corpora)
make nocase-bench        # a `nocase` field against a plain one over 100 000 rows: a put asking @unique, lookups, scans, `~`, an order, group and facet (ARGS=scan for some)
make collate-bench       # order ... collate tr over a million Turkish names, against PostgreSQL's tr-x-icu (that side after pgvector-up)
make ttl-bench           # @ttl: reads with and without an expiry, a sweep of 100 000 expired rows, one through a @hash of 5 values over 10M, a lone put and del beside such a field (`writes`)
make growth-bench        # indexes growing a put at a time, the longest (a table's growth), p50/p99.99: @unique to 4M keys, @text to 4M terms, @hnsw to 1.1M x 128 and 300k x 768; then builds, lookups, match, near, the first put after an open (GROWTH=unique|text|vector)
make analytics-bench     # bars, VWAP, distinct users, counts by bucket over 1M events and 1M ticks against the queries a client sent before; the fixed aggregates (`ARGS=old`: those alone, for another commit; `ARGS=writes` and `folds` the parts quoted below)
make counters-bench      # set {n: 7} against {n: n + 1}, 16 threads and 16 HTTP clients incrementing one key, the Redis recipes
make geo-bench           # a million points: radii, the nearest, through @geo and by the scan, against PostGIS and Redis in Docker (GEO_ARGS)
make queue-bench         # a job queue's claim over a million jobs: one worker, 16 threads, 16 HTTP clients, against read-then-set; QUEUE_ARGS=held (or "held pickup|delayed|idle|busy|other|contested|nearest"): a claim held by Fenec-Wait against polling, 100 and 1 000 held, busy queues beside held claims, a put of another collection beside claims held, one job to 10, 100 and 1 000 claims of their own, one order to couriers each claiming the nearest
make nearest-bench       # the nearest ready order claimed over HTTP, 1, 16 and 50 claimers: order distance(...) limit 1 returning id and an inner get's near, against examples/dispatch's read and /batch (ORDERS, CLAIMS)
```

Single tests:

```bash
cargo test -p fenec-core --test all persist::   # one file of a crate's integration tests (one binary)
cargo test -p fenec-ql near                      # by name substring (integration: fn name only)
cargo test -p fenec-core codec::tests            # inline unit tests in a module
cargo test -p fenec-core --no-default-features --features std-fs,language --lib --test features   # without the indexes
cargo test -p fenec-core --no-default-features --features std-fs --lib --test features   # without any feature
cargo test -p fenec-import --test all pg:: -- --ignored   # needs a live PostgreSQL
node --test web/fenec.test.js                    # JS: builder
node --test --test-name-pattern 'shape' web/fenec.sync.test.js
```

`make test` runs Rust first on purpose: `cargo test` builds the `fenec-server` binary
and `web/fenec.sync.test.js` runs against it (it self-skips when the binary or
`web/fenec.wasm` is missing).

**The wasm32 target comes from rustup.** Homebrew's `cargo` has no wasm32 std
library; the Makefile prefers `~/.cargo/bin/cargo` via `$(CARGO)`. Building by
hand needs `rustup target add wasm32-unknown-unknown`.

## Architecture

Dependency direction (nothing points back up):

```
fenec-core  (std only, zero deps)
     |
fenec-ql    (lexer + parser)          fenec-abi   (the answers both C ABIs give, and a replica's sync)
     |                                fenec-wasm  (browser C ABI)   fenec-ffi (native C ABI, apps)
     |
fenec-http  (REST/JSON + SSE, tenant registry, replication, /_metrics)
     |                     \
fenec-wire  (a PostgreSQL     fenec-shard (tenant router: directory,
     |      client: framing,             placement, move)
     |      SCRAM, COPY out, pgoutput)
fenec-import (SQLite file reader + PG COPY source + --follow)
     |                                  \
fenec-server (the server: HTTP, the      fenec-cli (`fenec` shell,
     syncer and shutdown, --follow        `fenec import`, `fenec types`)
     running the importer's follower)
```

`fenec-bench` is a measurement harness (`publish = false`) and is the only crate
allowed external crates — that is where `rusqlite`/`postgres` live.

`fenec-core` modules: `store` (segments, offset index), `declared` (a schema
declared in code: its description, the plan against a database), `engine` (`Database`,
`Collection`, replay/snapshot/compact/checkpoint), `vector` (HNSW + distance
kernels), `text` (tokenizer, inverted index, BM25), `highlight` (the spans a
`match`'s terms were read from), `query` (`Statement`, plan
execution), `schema`, `value`, `codec`, `collate` (ICU's root order and its Turkish
tailoring, generated tables in chunks),
`json`, `num` (decimal text to `f64` and back), `case` (a string's Unicode
case, as the standard library's, without its code), `time` (calendar arithmetic), `tz`
(the time zones `bucket` reads a clock by, IANA's rules since 2000), `sparse`
(sparse vectors and their inverted index), `changes`
(the change ring), `plugin` (registry), `sorted` (the ordered index),
`composite` (an index over several fields), `fuzzy` (the sorted words
`match ... fuzzy` and `prefix` walk), `geo` (points and `@geo`), `hll`
(HyperLogLog), `maps` (the hash maps the derived indexes are built into),
`history` (which history a database's writes belong to), `error`, `fs`
(buffered file I/O, behind the `std-fs` feature, as are `cache`, the page
cache's room, and `synclog`, the sync log), `off` (what stands in for an
index a build is made without).

The browser client is `web/fenec.js` — WASM glue (~605 lines), persistence and
the sync layer -- with the query builder (`web/builder.js`) and the HTTP client
(`web/http.js`) in dependency-free ES modules of their own, which it imports
and re-exports, and `web/client.js` the two alone (`@fenecdb/web/client`).
`web/fenec.d.ts` and `web/client.d.ts` hold the types; `fenec types <file>` generates schema-specific declarations.
`persist`/`restore` keep a database in IndexedDB as a file would hold it: an
image, then the writes since as chunks, from the journal `fenec_journal` starts
and `fenec_drain` empties (off until asked for -- a page that never drains would
hold every write). One row persists in 0.18 ms over 32 MB in Chrome, against 92
ms for the image. `openFile` (a dedicated worker) keeps the same bytes as a file
of the origin private file system -- the file `fenec-server` keeps, so each opens
the other's -- and `run` appends each statement's writes and flushes before it
answers: 0.56 ms a row, statement included, and it opens in 20 ms against
IndexedDB's 47 (`make file-bench`; Safari 0.98 and 0.36 ms). A new image
(`compact`, or appended writes past half the image and 64 KB) goes into
`<name>~` and is flushed before the file is written over, so a crash leaves
one of the two whole; `openFile` takes the copy when its image is whole.
`persist(db, key, { cryptoKey })` seals the image and each chunk with
AES-GCM (WebCrypto), a tag over the key, the image's random generation and
the chunk's number, so one moved, dropped from the middle or kept from an
image before is refused; a row persists in 0.16 ms either way, the 32 MB
image in 129 ms against 76. The OPFS file stays plain: it is `fenec-server`'s.

## Invariants worth knowing before you change things

### The crates

**[Zero dependencies is a hard rule](docs/design/crates.md#zero-dependencies)**
for every crate but `fenec-bench`; `fenec-core` dev-depends on `fenec-ql`.

**[Limits error, they do not truncate.](docs/design/crates.md#limits)**
`near`, `match` and `fuse` cap at 10 000 rows, expressions at 512 levels,
a `lookup` chain at 8 and `in (get ...)` at 100 000 values and 4 levels,
each a query error, never a cut result; a new limit takes that shape.

**[Profiles differ on purpose.](docs/design/crates.md#profiles)**
`fenec-cli` uses `cli` (`panic = abort`); `fenec-server` stays on
`release`: a connection's panic unwinds alone.

### Storage

**[No page cache, and the file is mapped.](docs/design/storage.md#the-mapped-file)**
`fs::open` maps it and a read decodes over its bytes; a rewrite points the
stores at the new file (`Database::repoint`).

**[Writes are handed over to the mapped file.](docs/design/storage.md#handover)**
Past `HANDOVER_AT` or `HANDOVER_DOCS`, `Database::hand_over` points the
stores at it, never with a block open, the file the same byte for byte.

**[A block past 16 MB spills into the file before it lands.](docs/design/storage.md#spills)**
A spill (kind 10) counts only where a land (kind 11) names it; the feed
sends the block as one record (`landed_block`).

**[A lone `create index` or `compact` runs beside the database](docs/design/storage.md#maintenance-beside-the-database)**
(`Database::maintain`): every write must pass `Database::note`, or a built
index misses it.

**[A file compacts itself once half of it is dead](docs/design/storage.md#compaction)**
and 64 MB: `CompactPolicy` is the one rule, run beside the database by
`Compactor` or `fenec_http::link` (`--auto-compact`), on replicas too.

**[A file is compacted before it outgrows the page cache](docs/design/storage.md#compaction-and-the-page-cache)**
(`CompactPolicy::memory`): a third dead suffices once the file nears
`cache::room` or the process faults.

**[File format](docs/design/storage.md#file-format):** a record is
`[kind][collection-id][length][body]`, its length read even when empty;
`head` alone writes a head, `record_at` reads it, `Walk` walks them. A torn
tail is cut on open, a torn image refused, and the file only grows.

### The engine's lock and blocks

**[Single writer, and readers beside it.](docs/design/engine.md#single-writer)**
Never two processes over one file: whatever writes one is a `fenec-server`
thread, and a block is open only under its opener's write lock.

**[A block's documents are built beside the readers](docs/design/engine.md#prepared-blocks)**
(`Database::prepare`, `early_put`), taken only where the collection holds
the schema `Arc` they were built against; all else is decided under the
write lock, as `tests/prepared.rs` holds.

**[A long read is pinned, and read with no lock held](docs/design/engine.md#pinned-reads)**
(`Database::pin`), sharing nothing a write changes in place; it declines a
read an index answers, and a pinned index refuses (`PINNED_OUT`), so a
stray read reruns under the lock.

**[Every write is a block, and a block is one record.](docs/design/engine.md#blocks)**
A failed one is undone in memory (`Undo`), whatever counts records counts
their writes (`writes_in`), and a block holds no compact.

### Durability

**[A storage error stops writes.](docs/design/durability.md#storage-errors)**
Writes after one are `Error::Io` until reopened (`Database::failure`). A
mapped file is replaced only by rename; a failed `fsync` is never retried
and runs after the lock (`Database::flush`'s `Durability`), its failure to
`Database::fail`.

**[Buffered writes are written behind the lock](docs/design/durability.md#writes-behind-the-lock)**
(`Database::write_behind`), asked after every write no fsync follows.

**[A durable write syncs a log beside the file, not the file](docs/design/durability.md#the-sync-log)**
(Linux, `synclog.rs`). Whatever renames a file into a database's place
removes the log first, durably (`fs::forget_sync_log`).

### Replication and archives

**[Replication ships only what is on disk, by the change counter.](docs/design/replication.md#replication)**
Every write goes through `wal`, its record moving the counter by its
writes; a record moving none is never sent, a durable write waits for
every stream (`Feed::wait_sent`), and a following database refuses writes
(`Error::ReadOnly`).

**[A node's tenants are archived over one stream](docs/design/replication.md#a-nodes-archive)**
(`fenec_http::node_stream`), a sender on each open tenant's feed:
`Tenants::close` refuses while it has a write left (`Feed::unsent`), and
whatever the stream does that can block sets `Sender::held` first, which
no durable write waits for; a sync waits for no stream
(`replication::unwaited`).

**[`/_changes` reads the writes on disk, documents and all](docs/design/replication.md#change-data-capture)**
(`Database::changes_in`): only what an fsync covered, never with writes
missing (410); a scoped token is refused.

**[A read can wait for a write on a replica](docs/design/replication.md#fenec-after)**
(`Hub::reached`): served once the database holds that change or 504
after `Fenec-Wait` (30 s at most), never before. Every wait comes after
its token (`admitted`), a stream's before its head.

**[An archive and a backup can be sealed](docs/design/replication.md#sealed-archives)**
(`seal.rs`, `--key-file`), never the live file.

**[`--follow` confirms nothing that is not on disk.](docs/design/replication.md#follow)**
Every write is a put or delete by id, so replays change nothing; the
mirror is a `fenec-server` thread, its collections' only writer. A wire
message's length is held to `proto::MAX_MESSAGE` before anything is
allocated (`proto::zeroed`).

### Tenants and leases

**[Scaling out is by tenant, one file each.](docs/design/tenants.md#tenants-and-leases)**
The tenant comes from the path, never the query, and the router forwards
bytes, parsing none; the registry opens a file under its
lock and closes it on its last `Arc` alone, never while its follower runs,
so no two `Database`s share a file. A tenant's role is its file's, not the
node's `--replica-of`. The router fails a node over on its own only under a
lease, which the node runs on the boot clock (`lease::Moment`) from the
grant the router acknowledged (`grant_from`); every write path meets the
engine's fence (`Database::set_fence`, in `commit` and `may_write`), every
route answering a write asks `answered_under_lease` (500 `outcome:
unknown` once lapsed), and a node promotes only a copy of the tenant's
lineage and history (`tenants::Expected`, 409).

### Regions and the global entry

**[A node is in a region and a zone, and a tenant in its node's region.](docs/design/regions.md#regions-and-zones)**
Its region is its node's, kept nowhere else; a copy stays in its primary's
region, another zone first (`Router::replica_node`).

**[A region may keep its own directory, and leases and fails over inside itself.](docs/design/regions.md#region-directories)**
A request is relayed once, never again (`regional_admin`), and a node heeds
its lease's holder alone, at a term no lower (`Lease::admits`).

**[One URL reaches every region: the Worker learns homes from the routers.](docs/design/regions.md#the-global-entry)**
It serves only `/t/<tenant>/`, never `/_replication`, decoding a path as
fenec-http does (`percentDecode`). It drops every address a client claims
and names it in `Fenec-Client`, an HMAC under `ENTRY_TOKEN` a router
believes only with `--entry-token` and within 60 s (`audit::entry`), and
drops from all it forwards (`forwarded_headers`). Every router answer carries
`Fenec-Shard: 1`; a request that may have reached one goes again only as a
`GET`, `HEAD`, `OPTIONS` or keyed.

### Read replicas

**[Read replicas in other regions answer only marked reads.](docs/design/read-replicas.md#read-replicas)**
The router parses no FenecQL: only `Fenec-Read: replica` goes near, and
every server runs a marked request read-only on every route (`marked`,
read in `serve_connection` alone); a failover never promotes a read
replica. A copy answers only once its primary said hello since it opened
(`Tenant::ready`, 503 otherwise) and only of its tenant's lineage
(`Follower::expect_lineage`), which every promotion and restore keeps
(`fork_id`). `Fenec-History` goes beside `Fenec-Seq`, and `after` waits for
that change of that history (`History::continues`); SDKs send home's
furthest position with each marked read. No lock is held across a call to
a node, and a refused hold makes none (`then`, never `then_some`).

### Access

**[A scoped token is held to its rules at every level, twice for
writes.](docs/design/access.md#scoped-tokens)** A new path that runs a
statement must go through `scoped()` or it reads everything, and a scoped
write run outside `within()` goes unchecked; a scoped stream tells
deletions only of ids it sent, and the algorithm is the key's, never the
token's.

**[A field a read rule withholds is held out of what a statement
does.](docs/design/access.md#field-lists)** `fields::hold` and
`fields::write` guard or refuse (403) each use of it; `tests/fields.rs`
holds each attack.

**[A rule may read another collection, answered before each
statement.](docs/design/access.md#rules-reading-another-collection)**
Its `in` lists are answered under each lock (`Scope::bound`), the scope
`Check`, the mask and streams use; a list past `MAX_CLAIM_VALUES` is
refused, never cut.

**[A token is bound to the tenant it
names.](docs/design/access.md#tenant-binding)** `route_tenant` asks it,
and no route below `/t/<t>/` may skip it; every tenant's database installs
the `Check` hook (`Tenants::check_scoped_writes`).

**[An empty token is no token.](docs/design/access.md#empty-tokens)**
Set empty, a token is unset and a key file refused; a new token reads
through `env_secret` or `env`, and a library constructor takes an empty
one as none (`Config::settled`).

### Quotas and retries

**[A tenant is held to its quota by its node, a block as it
lands.](docs/design/quotas.md#tenant-quotas)** A block or spill past
its bytes or rows is put back (507) as it lands (`Database::set_limit`),
one only taking away let through; a refusal names the bound alone.

**[A quota is a refusal where waiting makes no room, a wait the server
names otherwise.](docs/design/quotas.md#quotas-in-the-sync)** A synced
507 naming a `quota` is put back; a retry waits `retry_after_ms` where
longer than its backoff.

**[Every 503 says when to come back, in its body
too.](docs/design/quotas.md#retry-after-in-bodies)** Write one with
`Response::retry_after`: the sync reads bodies.

### The HTTP server

**[fenec studio is a page with no authority of its
own.](docs/design/http.md#studio)** Each statement is written in
`studio/statements*.js`, values as parameters; `make studio-highlight`
copies `site/highlight.js` in, and `transport.test.mjs` holds the in-page
answers to a server's.

**[An agent's MCP tool is a statement, held to the token as `/query`
is.](docs/design/http.md#mcp)** A model's filter goes in parsed, never as
text, and a read past `--mcp-max-rows` or `--mcp-max-bytes` is refused,
never cut.

**[An `Idempotency-Key` makes a write
once.](docs/design/http.md#idempotency-keys)** The write and its key's
`_idempotency` row are one block.

**[Refusals and schema changes are logged; a refusal
waits.](docs/design/http.md#audit)** Only `audit::verified`, where a token
checks out, clears the wait; a new check must call it. A node believes
`Fenec-Router` from the router alone; a test opening the log is a
`[[test]]` of its own.

**[Every request has an id, in its answer and every line it
wrote.](docs/design/http.md#request-ids)** `tests/datadog.rs` and
`tests/logs.rs` hold `monitoring/datadog/` to what is sent.

**[A request is a trace, sent over OTLP.](docs/design/http.md#traces)** A
thread-local from `begin` to `end`, `discard`ed before a stream takes the
connection over; a span names a statement's shape, never a value, and a
request never waits on the collector.

**[`/_metrics` counts at the edge, a shard per
thread.](docs/design/http.md#metrics)** Timed around `handle`, counted
before the answer is written; no label names a tenant or its collections,
and `--metrics` attaches no watcher.

**[`/_stats/statements` counts by shape, a tenant's
apart.](docs/design/http.md#statement-counts)** Literals are `$n`
(`statements::shape`); an unscoped token reads them, a tenant its own.

**[A statement is parsed once, and an answer goes out in one
write.](docs/design/http.md#parse-cache)** A JWT's scope is ANDed into a
copy, never the cached parse; an answer is one `writev`
(`http::write_parts`).

**[A plain `get` is written as JSON from the stored
documents.](docs/design/http.md#json-from-stored-rows)**
`Database::query_json` must match `query` through `json::rows_array_into`
byte for byte (`tests/query_json.rs`).

### The HNSW graph

**[The HNSW graph is derived data, not a
cache.](docs/design/hnsw.md#the-graph-is-derived)** Never written on the
write path; an open that cannot validate it rebuilds it silently, so a
corrupt graph loses no data. A tombstone carries its vector, and a rewrite
touches only indexes whose field changed, to the bit (`unindex_doc`).

**[A restore gives back the arena it had, to the
bit.](docs/design/hnsw.md#restored-arena)** Each vector is normalised in
the write path's order (`flat_sq`, `flat_sqs8`), or the graph would be
another.

**[A graph record is laid out flat.](docs/design/hnsw.md#flat-graph-records)**
Versions 9 (images) and 10 (tails) are written; every layout reads into
one set of arrays (`Read`), built in one place (`build_restored`).

**[A batch links in parallel, into the graph it would link in
turn.](docs/design/hnsw.md#parallel-linking)** Each list's additions go
in node order (`link_batch`).

**[The upper layers are walked with a
beam.](docs/design/hnsw.md#upper-layer-beam)** A search and an insert walk
each layer above 0 with `UPPER_BEAM` (4).

**[A block's `put`s link their vectors
together.](docs/design/hnsw.md#block-linking)** Natively they wait
(`defer_batch`), linked before the block's record is written.

**[A server answers before its graph is
linked.](docs/design/hnsw.md#deferred-linking)** `fs::open_serving`
leaves the tail's vectors for `link::beside` to link, a search measuring
each beside its walk so none is missed.

**[A server keeps its graphs in its
file.](docs/design/hnsw.md#graphs-in-the-tail)** `link::keep` appends a
changed graph (kind 4) under the read lock (`Database::save_graphs`); it
moves no counter and no replica is sent it.

**[A vector written again is one node.](docs/design/hnsw.md#aliases)**
Every insert, batch and deferral goes through `VectorIndex::place`: a
vector a live node holds to the bit makes its document an alias.

**[Natively the copies table grows from its own
slots.](docs/design/hnsw.md#the-copies-table)** `Same` grows reading no
vector, in 256 shards past `SAME_SPLIT`.

**[A graph record carries the copies table's
halves.](docs/design/hnsw.md#halves)** Every writer writes them
(`HALVES`), so a record is the same however its index was made.

**[The browser module hashes as a native build
does.](docs/design/hnsw.md#halves-in-the-browser)** It takes and writes
the halves, which `web/fenec.test.js` holds to a native record's.

**[An open copies a graph's vectors into the
arena.](docs/design/hnsw.md#opening-a-graph)** It restores and validates
the graph then.

### Benchmarks

**[`make scale-bench` holds fenec-server to pgvector, each over its own protocol.](docs/design/benchmarks.md#scale-bench)**
Sides run in turns after idle minutes; a hot run's figure is not one.

**[`make ycsb` is fenecdb as a general database.](docs/design/benchmarks.md#ycsb)**
`--verify` checks every answer; `ycsb/results.tsv` is appended to, never
rewritten (a replaced run named in `ycsb/superseded.tsv`), and
`site/build.py` holds every YCSB figure on the site to its median there.

### Writes

**[`insert` never writes over; `put`
does.](docs/design/writes.md#insert-and-put)** A held id is
`Error::Duplicate` (409); a scoped rewrite keeps `insert`.

**[A `set` reads the row it writes, and every write says what it
did.](docs/design/writes.md#arithmetic-in-set)** `query::arith` never
wraps past 64 bits; the record holds the result, never the expression.

**[`require <n>` makes a write's count a
condition.](docs/design/writes.md#require)** Any other count is
`Error::Unmet` (412), the block put back; `n` is an integer, never a
parameter.

**[`if absent else set` is an upsert; `put <c> $n` takes a parameter's
documents.](docs/design/writes.md#upserts)** Scoped, the row it sets
passes the update filter (`Check::before_overwrite`) and `$n`'s documents
go in first (`Statement::with_documents`).

**[A `set` or a `del` picks its rows with `order` and `limit`, and
`returning` answers them.](docs/design/writes.md#picked-writes)**
`page_ids` picks them under one lock; `returning` is held to the read
rules (`Hook::answering`).

**[A key of `order` may be an
expression](docs/design/writes.md#order-by-expression)** (`Sort::expr`,
`order_ids_by`), ties in id order; `check_order` refuses an inner `get`,
an aggregate or `expired()` in one.

**[A `put` answers the rows it wrote with
`returning`.](docs/design/writes.md#put-returning)** Each row reaches
`Hook::answering` as the write it was. A write clause goes into every
builder, held to the golden file, and the sync refuses one a replica
cannot answer (`PICKED`), as a scenario holds.

### FenecQL and schemas

**[SQL's spellings are FenecQL, read by the one
parser](docs/design/fenecql.md#sql-spellings)** into the same statements:
no mode, no second front end, no pg wire. What has no FenecQL meaning is
refused naming the FenecQL to write, without consuming its word; `"name"`
where SQL reads a column (`Parser::quoted`) and a regex-like `~`
(`regex_like`) are refused, `<>` stays `!=`.

**[`limit`, `offset` and `top` take a parameter, read where the plan reads
them](docs/design/fenecql.md#count-parameters)** (`Count::at`); a value is
refused as its literal is (`Select::check_counts`).

**[`in (get ...)` is answered before the query, as the list it
is.](docs/design/fenecql.md#inner-gets)** `Database::answered` puts an
`Expr::In` in its place on every path, so `eval` never meets one.

**[`alter collection` rewrites no document; positions are not
places.](docs/design/fenecql.md#alter-collection)** A drop keeps its place
until `compact`; `read_fields` takes places (`Schema::place`), never
positions.

**[A `json` field holds any value, and a path is a
name.](docs/design/fenecql.md#json-fields-and-paths)** A field name holds
no dot, so a path is an `Expr::Field`; a list of numbers read with no
schema is `f32`s, so `Database::exactly` must walk every place a statement
holds a value.

**[The shell runs a statement once nothing after it can go on with
it](docs/design/fenecql.md#the-shell)** (`fenec_ql::reading`). A new
statement form or lexer syntax owes a place in fenec-cli's `kind` (or a
write runs at Enter), a mark in `Parser::cut` wherever the text's end
decides a reading the parser backtracks over, `lexer::Unclosed` taught any
new string, comment or bracket, and a line in
`a_statement_written_over_lines_reads_as_one`'s corpus.

**[A schema declared in code is checked at every open, in the
engine.](docs/design/fenecql.md#declared-schemas)** `declared::plan`
plans additions alone, never guessing a rename; `make schema-golden`
writes `integrations/schema-golden.json`.

### Held writes

**[`Fenec-Wait` holds a `set` or a `del` that writes nothing until it
can](docs/design/waits.md#held-writes)** (`fenec_http::waits`, 30 s at
most), each run `handle` whole, token and all; a `put`, or the time read
but as a field against `now()` plus or minus a constant, is refused
(`holdable`).

**[A write wakes the held writes that read what it wrote, and a row
several groups would take goes to the one ranking it
first](docs/design/waits.md#waking-held-writes)** (`Waits::wrote`,
`waits::settle`). A landing that cannot name its collections wakes every
group (`None`).

**[One held request looks for every group, and a group that found nothing
looks among the rows written since](docs/design/waits.md#the-leaders-looks)**
(`waits::look_all`): a look among the rows written must pick as the whole
pick does, which runs wherever that is not shown.

### Indexes

**[`@unique` is a `@hash` that asks its bucket before a
write.](docs/design/indexes.md#unique)** `Collection::unique_clash` runs
after the hooks, before any write; a scoped token is told the field alone
(`access::told`).

**[A `@hash` bucket holds its ids ascending, in
runs](docs/design/indexes.md#hash-buckets)** (`engine/bucket.rs`), read in
order through `Bucket::iter`.

**[A hash index's table grows a 256th at a time, and a text index's terms
too](docs/design/indexes.md#sharded-tables)** (`maps::Sharded`): past
`SPLIT_AT`, 256 maps, each key kept beside its hash.

**[`@ttl` is an ordered index whose rows expire, and every read leaves them
out at once.](docs/design/indexes.md#ttl)** Every filter must have
`Database::alive` ANDed in; only a server sweeps (`fenec_http::sweep`),
and a scoped token reads `expired()` only by the `expired` grant.

**[Only an `and` chain reaches an index.](docs/design/indexes.md#index-reach)**
An `in` takes buckets only when every element resolves, else the scan.

**[A filter is bound to its collection once a
query.](docs/design/indexes.md#bound-filters)** What `Filter` cannot bind
stays `eval`: an error at the first row, never over none.

**[`@sorted` must give the scan's answer, row for
row.](docs/design/indexes.md#sorted)** Keys order as `Value::cmp_value`,
`null` and `NaN` apart, ties in id order; `tests/sorted.rs` holds it to a
twin without the index.

**[An index over several fields is its parts' bytes laid end to
end](docs/design/indexes.md#composite-indexes)** (`composite.rs`), a
part's bytes ordering as `cmp_value` orders its values or held apart;
`tests/composite.rs` holds every filter, order and page to a twin without
it.

**[An open builds no hash, text, ordered or sparse
index.](docs/design/indexes.md#lazy-indexes)** The first read fills its
`Derived`; a write skips an unbuilt one.

**[A server warms what an open left for the first
read](docs/design/indexes.md#warm)** (`--warm`, `fenec_http::warm`), an
index at a time under the read lock; a bench's warm-up reads every index
its writes keep up.

**[A point is two `f64`s on Redis's sphere; `@geo` is Morton
keys](docs/design/indexes.md#geo)**: `distance` uses `geo.rs`'s own
series, never libm, so a server and a page agree; a list bound for a
point is read as written (`DataType::keeps_numbers`).

### The browser module

**[Threads are `cfg`'d out of WASM.](docs/design/browser-module.md#no-threads-in-wasm)**
Nor has it a clock: `now()` reads the time a statement is handed
(`Database::set_clock`), or errors.

**[Indexes are features; a build without one opens a file declaring
it.](docs/design/browser-module.md#index-features)** A missing one is a type
of no value (`off.rs`), and a replayed `create index` of one is taken
unbuilt, never refused; a crate using `fenec-core` names `full`, and
`fenec-ql` names it in its dev-dependencies alone.

**[The rest of the language is features
too.](docs/design/browser-module.md#language-features)** Its checks ask
`crate::FULL` first; a build without one opens every file, refusing what
it cannot answer exactly (`refuse_unbuilt`). `FENEC_WRITE_FIXTURE=1`
rewrites `tests/language.fenec`.

**[What is core, and why one module
ships.](docs/design/browser-module.md#what-is-core)** What any app expects
is core, a domain's extra a feature.

**[The module holds one copy of a
generic.](docs/design/browser-module.md#one-copy-of-a-generic)** Sort
through `sort_unstable`, `text::best_first` or `order_rows`; slice a `str`
with `get`, never `s[a..b]`.

**[The core lost what it held twice or for one
use.](docs/design/browser-module.md#size-cuts)** What std brings in for one
call is the crate's own (`num::parse_i64`).

**[The module grows nothing by doubling past a
mebibyte.](docs/design/browser-module.md#chunked-memory)** Its memory never
goes back, so vectors are 1 MiB chunks (`ROWS_CHUNK`) and a segment
doubles only to its sealing record.

### Vector search

**[wasm32's SIMD kernels match the scalar ones bit for
bit.](docs/design/vector-search.md#simd)** `vector::simd` keeps the
scalar loop's order (`web/fenec.test.js`).

**[aarch64's strips are NEON, to the same
bits.](docs/design/vector-search.md#neon)** `vector::neon` multiplies and
adds unfused in `strip8!`'s order, held by
`f32_and_f16_kernels_match_the_scalar_strips`.

**[A vector goes out as its
`f32`s.](docs/design/vector-search.md#vectors-out)** JSON writes the
shortest text reading back as it (`json::num32_into`), but for
`json::TIE`.

**[A number is read exactly.](docs/design/vector-search.md#numbers-in)**
`num::parse_f64` reads one, and natively `num::clinger` a list of numbers
alone into `f32`s.

**[A vector parameter goes into the module as
`f32`s.](docs/design/vector-search.md#vector-parameters)** `run` hands it
beside the JSON (`vectorsApart`), `-0` as `0`.

**[A quantized index holds codes, and `near` orders by the
vectors.](docs/design/vector-search.md#quantized-indexes)** `Space` orders
the beam by the stored vectors, so every score is exact.

**[A bit code is a residual's signs from a learned
centre.](docs/design/vector-search.md#bit-codes)** Its centres are
learned alike on every target and travel in the graph record
(`BIT_CENTRED`).

**[Filtered `near` needs its
fallback.](docs/design/vector-search.md#filtered-near)** A filtered ANN
walk short of the limit must search the whole set, or a correlated
filter answers empty (`tests/filtered.rs`).

**[`sparse<N>` is pgvector's `sparsevec`, and `@inverted` answers
exactly.](docs/design/vector-search.md#sparse-vectors)** Every way in goes
through `sparse::normalise`, which the index relies on.

### The JS client and Cloudflare

**[One module and a client entry, by where the queries
run.](docs/design/js-client.md#the-module-and-the-client)** `fenec-lite`
and `fenec-min` are test builds, never shipped; `web/client.js` is
`builder.js` and `http.js` alone (`web/fenec.client.test.js`).
`FenecHttp.live`'s streams and polls stay home, its runs read near.

**[A Durable Object keeps a database as a file
would.](docs/design/js-client.md#durable-objects)** `persist` writes an
image, then a row a flush (`storage.sql`), a new image and the old's drop
in one `transactionSync`; a failed flush stores nothing and rejects every
later `persist`.

**[An object's alarm keeps its
database.](docs/design/js-client.md#alarms)** `schedule` sets one only when
something is due; `maintain` returns a failure as `error`, never a
rejection; every database let go goes through `#retire`, a released one
refusing `restore`.

**[A live query learns what changed from the change
ring.](docs/design/js-client.md#live-queries)** `Query.reads` names each
collection read (`Lives`), `fenec_changes` those written.

### Text

**[`collate und` and `collate tr` are ICU's
orders.](docs/design/text.md#collations)** `tools/collate/gen.py` writes
the tables, and equality stays the bytes'. In
the browser a statement meeting a chunk it lacks is refused
(`collate::refuse`), a write before it changes anything.

**[`nocase` text compares by its case fold wherever it
compares.](docs/design/text.md#nocase)** Every such place goes through
`Field::text_order`; the fold is `str.casefold()`'s (`tests/nocase.rs`),
and a field made `nocase` or no longer is a `rebuild`.

**[`match` prunes with MaxScore.](docs/design/text.md#maxscore)** Scores
add in `f64`; `pruning_never_changes_the_answer` holds it to an
exhaustive merge.

**[`@text` matches whole words unless told
otherwise.](docs/design/text.md#tokenizer)** It folds `I`, `İ`, `ı`, `i`
together; unspaced scripts go by grams (`text::grams`).

**[The text index is derived and never
persisted.](docs/design/text.md#text-index-is-derived)** Nothing of it
reaches the file.

**[A text index's words are a table of their own
(`text/terms.rs`).](docs/design/text.md#term-table)** `memory_bytes`
counts each list's allocation past its bytes (`ALLOC`).

**[`fuzzy` and `prefix` walk a sorted copy of the
words.](docs/design/text.md#fuzzy-and-prefix)** Every write keeps it up
once built, never in the file; a word reaching more than
`MAX_EXPANSIONS` (10 000) words is refused, never cut.

**[`rerank` deliberately uses no index.](docs/design/text.md#rerank)** It
reorders `match`'s candidates by exact distance over stored vectors.

**[`fuse` adds ranks, not scores.](docs/design/text.md#fuse)** Each search
runs `candidates` deep, filtered; a row scores `1 / (k + rank)` from
each, `k` 60.

**[`highlight()` re-reads the text; `facet` counts beside the
rows.](docs/design/text.md#highlight-and-facet)** Marks are the searched
index's tokens; a facet past `MAX_FACET_VALUES` (10 000) without `top` is
refused, never cut.

**[A disjunctive facet's filter is split as
written.](docs/design/text.md#disjunctive-facets)** `scoped()`, `@ttl`'s
`alive` and anything else ANDed in after must reach every split filter
through `each_filter_mut`.

### lookup and aggregates

**[`lookup` chains, and the chain is still
positional.](docs/design/lookup-and-aggregates.md#lookup-chains)** `on
child = parent` names the level just above.

**[`lookup` is one bucket probe per parent, not a
join.](docs/design/lookup-and-aggregates.md#lookup)** An unindexed child
key is refused; nesting is never a `Value`.

**[`required` is the other half, and it is a
pass.](docs/design/lookup-and-aggregates.md#required)** It drops a parent
no child matches before `offset` and `limit`.

**[A `lookup` over a key of several fields is a run of an index over
several fields.](docs/design/lookup-and-aggregates.md#lookup-over-several-fields)**
`tests/composite_lookup.rs` holds it to plain `get`s.

**[A `lookup` level may fold its children, and a `lookup` hangs off
groups.](docs/design/lookup-and-aggregates.md#folded-levels)** A level is
the `get` its children would be; `tests/lookup_aggregate.rs` holds it.

**[An item of a select list is an expression, and an aggregate folds
one](docs/design/lookup-and-aggregates.md#select-expressions)**: an
aggregate is a call, never an `Expr` variant, and `count(distinct)` past
`MAX_DISTINCT_VALUES` is refused.

**[`approx_count_distinct` is HyperLogLog, and `having` keeps
groups](docs/design/lookup-and-aggregates.md#hyperloglog-and-having)**
(`hll.rs`): its own hash, the same on every target, so sketches merge.

**[`bucket(at, 1d, 'Europe/Berlin')` reads a zone's
clock](docs/design/lookup-and-aggregates.md#time-zones)** (`tz.rs`):
`tools/tz/gen.py` writes `tz/zones.bin`; before 2000 a named zone is
refused.

### The native library and sync

**[An app keeps its database through
`fenec-ffi`.](docs/design/native-and-sync.md#the-native-library)** A handle
indexes a table of `Arc`s, never a pointer; every call returns a code
under `catch_unwind`; `include/fenec.h`, by hand, is held to the exports
by a test; `<file>.lock` is an `fcntl` record lock, never `flock`.

**[A replica's sync is a state machine; the network is the
binding's.](docs/design/native-and-sync.md#sync-core)** `fenec_abi::sync`
does no I/O: a binding feeds it events in turn and performs its actions.
Only `fenec-ffi` turns on `fenec-abi`'s `sync` feature; the browser module
has none of it.

**[Both sync runners are held to one scenario
file.](docs/design/native-and-sync.md#sync-scenarios)** A behaviour of
either changes in `integrations/sync-scenarios.json` first; `make
sync-scenarios-check` fails unless both ran every scenario.

### The router

**[The router tells a page one CORS rule, its
own.](docs/design/router.md#cors)** With `--http-cors` it answers
preflights and puts its own CORS headers in place of a node's
(`cors::is_cors_header`), never both.

**[The router looks at a pooled connection first, and resends only what
cannot have run or may run twice.](docs/design/router.md#pooled-connections)**
A request not sent whole goes again, one sent whole only if `replayable` (a
`GET`, `HEAD`, `OPTIONS` or keyed), judged by `Stage`, not errno. Every
dial goes through `dial::resolve`; every body read allocates through
`zeroed` and refuses an answer with no length.

**[A failover ends what the router has out to the node for the tenants
it moved.](docs/design/router.md#failover-cuts)** `promote_on` has
`Pool::cut` shut what is out to the node under the tenant's keyed mark
(`mark_of`), never an answer begun (`ANSWERING`); a cut request follows
`replayable`'s rule, and a forward reads `Pool::moved` before the
directory and before its first byte. Only a failover cuts.

**[A query across tenants is the router's admin's, merged as the request
says.](docs/design/router.md#queries-across-tenants)** Nodes run it
read-only; the router parses no FenecQL, fails and names a tenant its
merge cannot hold exactly, and refuses (413) past its bounds, never cuts.

### Integrations and deploy

**[`integrations/` may use outside packages; the crates may
not.](docs/design/integrations.md#integrations)** A builder change starts
in `web/builder.js` and a case in `golden.mjs`, then every SDK's follows,
held to `integrations/builder-golden.json`; `make types-check` holds
`fenec.d.ts` to `fenec.js`. An SDK bounds a request by its context or
token, never the HTTP client's timeout; a store checks its names against
FenecQL's and passes values as parameters.

**[Grafana's data source binds a dashboard's values; it never splices
them.](docs/design/integrations.md#grafana)** Names become `$n`s
(`fenecql.go`); only `get`, `select` and `explain` go out.

**[`deploy/` is the regions' kit, held to the binaries by two
checks.](docs/design/integrations.md#deploy)** Tokens go in env files or
`LoadCredential=`, never on a command line, a role a user each; no
`ProcSubset=pid`. The tunnel passes only tenant paths, never
`_replication` (`deploy/test/ingress.py`); the bucket copy is `rclone
copy`, never `sync`; `tests/datadog.rs` holds the alerts to scrapes.

## Conventions

- `Error` (`fenec-core/src/error.rs`) is the single error type: allocation-free
  variants, no `Box`. `fenec-http` maps it onto HTTP statuses (`api::status_of`).
- Unit tests live inline in `#[cfg(test)] mod tests`; cross-crate and protocol
  tests live in `crates/*/tests/`, a file each, gathered into one binary a
  crate by `tests/all.rs` (`autotests = false`: a new file needs its `mod`
  line there, or it is never built). One that reads the process's own
  counts, `/_metrics` or the statements', is a `[[test]]` of its own:
  beside the others it would count theirs. Measurement programs are
  each crate's `examples/` (most of them `fenec-core`'s) and `fenec-bench`'s
  binaries, and are wired to `make` targets, not to CI.
- The wait after a refusal is the process's, so the servers of a test binary
  that holds many tests ask with none (`audit::set_delay(0)` in their
  helpers): every test asks from 127.0.0.1, and their refusals together
  held one test's 401 past its read timeout. `fenec-http`'s
  `tests/audit.rs` and `fenec-shard`'s `tests/refusals.rs` hold the wait,
  each a `[[test]]` of its own: which count a refusal went to is read off
  the count (`audit::would_wait`), and a request's time is bounded from
  below only. macOS gives the timers of a process whose QoS it clamps
  about 200 ms of leeway, and a runner's first refusal, a wait of 50 ms,
  took 256.6 against a bound of 200.
- A test that fails and passes on a rerun is a bug, the test's or the code's:
  it is reproduced -- in a loop, under load (`docker run --cpus=3` beside
  busy loops is GitHub's three-core runner) -- and fixed, never retried
  away or given a longer sleep. A test waits for an event, not for a time:
  it holds what it races (`Replica.setOnline(false)` before a write it
  reads back, a call held under way across a burst, a disk's fsync held at
  a gate) and bounds only the wait for something that must happen. A CI
  log names every failing test with its assertion: cargo's and `swift
  test`'s own output, Gradle's `testLogging` (set for `-q`'s quiet level
  too), `dart test`'s GitHub reporter, `node --test --test-reporter=spec`.
  A failing mobile job also uploads `test-reports-android` (the JUnit
  reports) or `test-results-apple` (the simulator's `.xcresult`) for a
  week.
- A test reads the clock from below. A sleep never ends early, so `took >=
  wait` holds on any machine, while a bound from above measures the machine:
  macOS gives the timers of a job whose QoS it clamps hundreds of
  milliseconds of leeway (`taskpolicy -c background` is the way to try
  it), and a container of three cores beside busy loops stood still for
  2.7 s. Where the time is the property -- a held request answered before
  its wait, a node that stops once its lease is out -- the bound is what it
  stands against, with seconds to spare: half the wait, the lease the node
  reports it has left, never a guess at how soon the system runs a thread.
  Anywhere else the test asserts the event, the count or the order: the
  wait a refusal would take (`audit::would_wait`), a claim still held when a
  freeze returns, another tenant answered while a slow open is held at a
  gate, a grant queued behind another (`Router::grants_waiting`). A timeout
  a test gives its servers or clients bounds every request they make, not
  just the one it is about: a router's upstream timeout of 600 ms failed a
  tenant's create at background QoS, and an SDK client's 100 ms left
  around a held claim's 400 failed nearly every run there.
- A server a test or a harness starts listens on port 0 (`--http
  127.0.0.1:0`, `--listen 127.0.0.1:0`, Node's `listen(0)`) and is found
  by the address it says it took: fenec-server's and fenec-shard's
  `listening on: http://<address>` line on stderr, which both binaries'
  usage names and `fenec-shard`'s `tests/listen.rs` holds. A fixed port was
  "Address already in use" beside another suite on the machine (the
  examples' clusters), and a port found free and handed over was free only
  until someone else took it, a health check then answered by another's
  server. A router is told its nodes by the addresses they said. Only a
  site whose address is needed before it binds -- a Next.js build renders
  it, the database's CORS names it -- takes a port the system picks held
  by a stand-in listener until the moment it starts (`examples/shop`,
  `examples/dispatch`'s `ci.sh`).
- A change that adds or alters something a user meets says so where a user
  looks, in the same pull request: its reference page under
  `site/content/docs/`; README's feature table and the links beside it;
  the home page (`site/content/index.html`) and the docs index when the
  feature is one a visitor would pick fenecdb for; and the SDK lists --
  `languages.html`, `mobile.html`, `sharding.html`'s SDK paragraph,
  `integrations.html`'s -- when an SDK changes. Every reference page kept up
  with regions, the global entry, the node's archive and read replicas
  (#318 to #333) while README and the home page named none of them. A
  section added or changed is found by its words alone until the site's
  search vector cache has its vector: Workers AI runs in the `site` deploy
  alone, which uploads the cache as it left it (the `search-embeddings`
  artifact), so after sections change, the next deploy's artifact is
  committed as `site/search-embeddings.json` (`gh run download <run> -n
  search-embeddings`) where it differs.
- An invariant's rule goes in CLAUDE.md in a few sentences: what must
  hold, what must never be done, what enforces it and what a change owes.
  Its measurements, rejected alternatives and history go in its
  `docs/design/` file, in the same pull request; a new invariant gets both.
- Comments explain *why* a thing is the way it is — a measured cost, a trap that
  was hit, an alternative that was rejected. Match that when adding code.
- All prose in the repo (comments, docs, README) is English.
