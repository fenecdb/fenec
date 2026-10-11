# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

fenecdb — a minimal, vector-native embedded database in Rust. Compiles to WASM for
the browser, has its own query language (FenecQL), and its server (`fenec-server`)
speaks HTTP; every language reaches it that way, Python, JavaScript, Go and
.NET through official clients. The docs under `site/content/docs/` are the long-form reference (design
rationale, benchmarks, full FenecQL and HTTP surface); `README.md` is the
front door and links into them, and this file is the working summary.
`AGENTS.md` is this file for other agents, the same text under its own title
and first line: `make agents-md` writes it from `CLAUDE.md`
(`tools/agents_md.py`), and CI fails when the two differ, so edit `CLAUDE.md`.

## Commands

```bash
make test          # cargo test (no fenec-bench, no examples), fenec-core without its indexes, then the JS tests
make wasm          # builds fenec-wasm for wasm32, copies to web/fenec.wasm
make wasm FEATURES="text sorted"   # without the other indexes (FEATURES=none: none of them)
make wasm-lite     # the test build without any, to web/fenec-lite.wasm (web/fenec.test.js; shipped by nothing)
make wasm-sizes    # the module's size with each of the 16 sets of indexes
make size-report   # where the module's bytes go, by crate, module and std (BASE=main: against main; BIN=fenec-server: a native binary's)
make wasm-speed    # the module in Node: HNSW build, near, filter, match, JSON (speed.mjs a.wasm b.wasm compares builds)
make wasm-exact-speed   # near without the graph (the wasm-lite build), 1k-50k rows x 128/384, against the graph and exact
make ffi           # the native library for apps (crates/fenec-ffi) for this machine, TARGET=... another, JNI=1 with the Kotlin functions
make ffi-bench     # a call through the native library against fenec-server's handler in process: open, put, near
make sync-bench    # the sync core (fenec_abi::sync) a change applied, against the same put alone
make sync-scenarios-check   # both runners of integrations/sync-scenarios.json passed every scenario (after make test)
make swift-test    # the Swift package (Package.swift) on macOS: the XCFramework's macOS slice, swift test (sync tests start a fenec-server), then again on one cooperative thread
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
make docs-types   # every data-lang="ts" example on the site under tsc --strict (web/types/docs.mjs; part of make types-check)
make react-test    # useLiveQuery vs a real fenec-server replica (needs `make wasm`)
make studio-test   # fenec studio (--studio) in headless Chrome: browse, edit, a scoped token, 100k rows
make grafana-check # monitoring/'s compose (Docker): every panel of the data dashboard (Infinity) asked of Grafana, held to fenec-server's answer
make beir BEIR=dir # nDCG@10 per ranking path (vectors: crates/fenec-bench/beir, embed.mjs + splade.mjs; BM25 alone without; FENECBENCH_TEXT=chars sets @text's options)
make import-test   # the PostgreSQL arm of import and --follow (needs Docker)
make follow-bench  # --follow: commit-to-visible latency, drain, reconnect (pgvector-up first)
make mirror-bench  # fenec-server --follow: commit to a subscriber, a server killed and started again
make small         # smallest `fenec` binary: --profile cli --no-default-features
make server TOKEN=secret HTTP=127.0.0.1:8080   # run the server against ./data.fenec
make node ADMIN=secret   # a tenant node: fenec-server --dir tenants
make shard               # the router in front of the nodes (./shard.fenec)
make shard-bench         # router overhead per request, tenant move time, failovers by hand and on a lease
make replica-bench       # replica lag per sync policy, catch-up, what a failover loses
make concurrency-bench   # writers and readers at once against SQLite: durable and buffered writes, reads beside blocks of writes
make requests-bench      # a request over HTTP: one client's round trip, eight's rate, against PostgreSQL
make recon-bench         # a ledger's reconciliation /batch beside transfers and reads: their longest waits
make roundtrip-bench     # one client's round trip taken apart: the client, the server's phases (--features timing, GET /_timing), PostgreSQL's bind and execute
make load-bench          # loading 100 000 rows each way a client can send them, against PostgreSQL's COPY and INSERT
make maintenance-bench   # reads and writes during create index / compact
make compact-bench       # a file under updates, compacted on its own or not: its size, reads during and after each compact, its swap's lock (COMPACT_ARGS)
make open-bench          # opening a 1 GB file, read into memory or mapped
make reopen-bench        # a crashed 100k x 768 file: linked at the open, beside the queries, or with its graphs kept
make quant-bench         # quant=int8|bit against full vectors: memory, recall, latency
make scale-bench         # fenec-server over HTTP against pgvector at scale: load, memory, recall, latency, filters (pgvector-up first)
make ycsb                # YCSB A-F: fenecdb vs SQLite in process, fenec-server vs PostgreSQL and MongoDB in Docker, durable and buffered (YCSB_ARGS)
make statements-bench    # what counting a statement by its shape costs
make subquery-bench      # in (get ...) against its list written out and against lookup ... required
make search-bench        # highlight(), snippet() and facet over 100 000 documents: a row's marks, a facet by buckets and by scan, by ranges, disjunctive
make ttl-bench           # @ttl: reads with and without an expiry, a sweep of 100 000 expired rows, one through a @hash of 5 values over 10M
make growth-bench        # indexes growing a put at a time, the longest (a table's growth), p50/p99.99: @unique to 4M keys, @text to 4M terms, @hnsw to 1.1M x 128 and 300k x 768; then builds, lookups, match, near, the first put after an open (GROWTH=unique|text|vector)
make analytics-bench     # bars, VWAP, distinct users, counts by bucket over 1M events and 1M ticks against the queries a client sent before; the fixed aggregates (`old`: those alone, for another commit)
make counters-bench      # set {n: 7} against {n: n + 1}, 16 threads and 16 HTTP clients incrementing one key, the Redis recipes
make geo-bench           # a million points: radii, the nearest, through @geo and by the scan, against PostGIS and Redis in Docker (GEO_ARGS)
make queue-bench         # a job queue's claim over a million jobs: one worker, 16 threads, 16 HTTP clients, against read-then-set
```

Single tests:

```bash
cargo test -p fenec-core --test all persist::   # one file of a crate's integration tests (one binary)
cargo test -p fenec-ql near                      # by name substring (integration: fn name only)
cargo test -p fenec-core codec::tests            # inline unit tests in a module
cargo test -p fenec-core --no-default-features --features std-fs --lib --test features   # without the indexes
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
     |
fenec-server (the server: HTTP, the syncer and shutdown,
     |        --follow running the importer's follower)
fenec-cli   (`fenec` shell, `fenec import`, `fenec types`)
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
case, as the standard library's, without its code), `time` (calendar arithmetic), `sparse`
(sparse vectors and their inverted index), `changes`
(the change ring), `plugin` (registry), `fs` (buffered file I/O, behind the
`std-fs` feature), `off` (what stands in for an index a build is made
without).

The browser client is `web/fenec.js` — WASM glue (~545 lines), persistence and
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

**Zero dependencies is a hard rule** for `fenec-core`, `fenec-ql`, `fenec-abi`, `fenec-wasm`, `fenec-ffi`,
`fenec-http`, `fenec-wire`, `fenec-server`, `fenec-import`, `fenec-shard`. The WASM output has to stay small and
auditable; own codec, own JSON, own HNSW, own SCRAM/crypto, own decimal-to-`f64`
and back (`str::parse` drags in a 12 KB table, `{}` on a float 20.8 KB of
Grisu and Dragon -- see `num.rs`). `fenec-core` does
dev-depend on `fenec-ql` (Cargo allows the cycle through a dev dependency) so tests
can write real queries.

**No page cache, and the file is mapped.** The byte sequence on disk and in
memory are the same format, so a read decodes straight over the bytes --
there is no eviction policy, no dirty pages and no cache of fenecdb's own.
`fs::open` maps the file where the target maps files (unix, 64-bit), so the
documents stay in it and the process holds what it derived from them: the
offset index, the hash, ordered and text indexes, the graph, and the
documents written since the open until it hands them over to the file
(below). A 1 GB file of 2.3 million rows with a hash and an ordered
index holds 188 MB that way against 1 095 read into memory, and its
`compact` peaks at 236 MB against 2 012; a 10 GB file opens on an 8 GB
machine, which read it cannot. `fs::open_in_memory` (`fenec-server --no-mmap`,
which reaches a replicated file and a `--dir` node's tenants as well) is the
other way, for a network file system or to have `--max-memory` cover the
data. In the browser `store::Base` is the image a load was handed
(`fenec_load_owned`, `Database::load_mapped`): the module keeps it and
reads the documents out of it, as a server reads its file, where it copied
each into a segment -- 10 000 rows of 768 dimensions restored held 97 MB,
and hold 66, for 0.7 KB brotli; the load is up to 12% quicker and the
first build of each derived index after it 2 to 6% slower, which grows the
memory past the image the copy let go of: a load and its first three reads
took 28.7 ms against 28.8. A
new file is mapped from the start. A rewrite -- `checkpoint`,
`compact`, an image adopted -- writes the new file and points the stores at
it (`Database::repoint`), so the old one is let go of; a compact over a
mapped file never copies a record into memory, on a server either, and
rebuilds no index but a graph holding tombstones, since the documents are
the same ones. An image this database writes records where each
collection's data went (`image_into`'s `placed`), and each store works its
new places out from its own (`Store::relocate_image`, `relocate_live`):
walking the new file's record heads read the whole of it back, 1.8 to 2.3 s
of a 1 GB checkpoint under the write lock, against 19 ms. Only an adopted
image, written elsewhere, is walked.

**A mapped database hands what it writes over to its file.** A block's
record goes into the file as it lands, and the stores held its documents in
their segments besides, until a restart or a compact: 250 000 768-dim rows
loaded into a server, the engine counted 1 592 MB against 818 for the same
file started again. Once the documents since the last handover amount to
`HANDOVER_AT` (16 MB), 65 536 records or 65 536 writes
(`HANDOVER_DOCS`: its pause is a document's work each, about 17 ns, and 16
MB of rows of a text and an int were 930 000 of them, the write lock held
16 to 17.5 ms), `Database::hand_over` has the sink
write what is pending (`Sink::written_through`) and each store point the
documents it holds at their places in the file and let its segments go
(`Store::hand_over`): 820 MB after that load. One due while a durability
fsyncs the file is put off to the first write landing after it, up to
twice the bounds (`Sink::syncing`): it writes into the file, and waited
out the fsync with the write lock held (below). Where each record went is
noted as it lands (`handover::Landed`: a data record's body, each of a
block record's), so nothing of the file is read, and a store takes its runs
in only when they account for every frame its segments hold, in order --
frames a compact built in memory stay, and with them the ones after. Nothing
is handed over while a block is open, whose writes are in no record yet.
The file is the same byte for byte, and so is the image a checkpoint writes
from it (`tests/handover.rs`). The write lock is held 1 ms for 16 MB of
768-dim documents and 2 ms of 128-dim ones; 3.9 and 8.5 at 64 MB. The file
is mapped with room past its end (`Mapping::with_room`: its length again, a
gigabyte at the least, address space alone) and the mapping grows over the
appends, which Linux and macOS both show through a mapping made before them,
so readers keep the pages they touched; only a file outgrowing it is mapped
anew. Whether the heap goes back to the system is the allocator's call:
musl's and glibc's unmap a large block when it is freed, so the process
holds what the engine counts (250 000 x 768 with no graph: 6 MB of
anonymous memory against 777), while macOS's keeps hundreds of megabytes of
freed large blocks in a cache of its own, which its footprint counts (998
to 1 288 MB after the 250 000 x 768 load; 823 with `MallocSpaceEfficient=1`).
So a large block's buffers are let go of as it lands (`Database::spare`),
where a COPY of 50 000 768-dim rows kept its 154 MB of frames until the next
write, and a record the sink's buffer cannot hold is written where it lies
rather than copied into it.

**A block that outgrows 16 MB spills into the file before it lands.** Held
back until it landed, a block's frames were in memory twice, in its record
and in the stores: 250 000 768-dim rows in one block peaked at 2 389 MB of
anonymous memory on Linux against the 820 they left, blocks of 50 000 at
1 144. Once the frames a block holds amount to `SPILL_AT` (16 MB),
`run_one` has it spill after the statement (`Database::spill`): the frames
are appended as a spill record (kind 10, a block record's body), and each
store takes its frames in from there with the records that landed before
the block (`Store::hand_over_keeping`) -- the block's mark for it becomes
where it stood then, those taken in (`Mark::spilled`), and each document
the block overwrote is pointed at where it is in the file now
(`store::Moved`), so a rollback puts it back. The block lands as a land
record (kind 11) naming its spills by where their bodies are, then its
writes since the last; a load applies a spill only where a land names it,
and cuts off the spills a file ends with. A rollback, a lapsed lease or a
crash leaves them dead in the file until a compact. The land never travels:
the sink is handed the spills' bodies (`Sink::land`), and a primary's feed
sends the block as the one block record it would have been
(`landed_block`), which a replica applies whole. A spill that cannot be
made (`Store::would_hand_over` refuses its frames) stops the block's
spills, which lands holding them as before. With them the peaks were 904 and
888 MB (in process, in a `rust:alpine` container), and the one block
without a graph loaded in 3.8 s against 8.6. A binary from before refuses
kinds 10 and 11. The browser module writes no spill, and reading them cost
it 1.5 KB, 0.5 KB brotli.

**Single writer, and readers beside it.** Reads take a shared lock
(`Database::query`), writes the exclusive one (`execute_with`), and a block
of writes (`begin` .. `commit`) is open only under the write lock of
whoever opened it: a `/batch`, a keyed write, the browser module's `run` of
several. So a read lock finds no block open and reads what has landed, and
`fenec_http::held` is only the lock taken whole past a poisoned one. A pg
transaction once left its block open between statements, parked for
readers (`leave_block`, `park`, savepoints); with the pg wire gone that
machinery went, since nothing else leaves a block open. What one writer
costs readers is measured against SQLite in one process (`make
concurrency-bench`): four threads read 7.1M rows/s by id alone, 701k beside four writers, and
213k beside blocks of 1 000 writes landing back to back, p99 0.51 ms (a
block's run) and the longest 4.8 -- a block holds the write lock while it
runs, where SQLite's WAL reads the last commit meanwhile, 632k/s and 0.6 ms
at most; before the handover counted writes the longest was 17.4, a handover
of 930 000 small documents. Durable writes gain from the fsync outside the
lock: 252 -> 516 writes/s from 1 to 16 writers, SQLite's 248 -> 258. Two processes opening the same file corrupts
it, which is why everything that writes one -- the HTTP endpoint, a
replica's follower, the graph keeper, `--follow`'s mirror -- is a thread
of `fenec-server`, never a binary of its own. A `/batch` whose every
statement reads (`read_batch`) is one snapshot, as the native library's
`fenec_abi::query` is: every read at the one change `Fenec-Seq` names, a
`require` stopping it with 412 and `at`. It took the write lock as every
batch did, then the read lock once for all of them (reads by id with no
transfer waiting, 18 waits past 50 ms -> 2), and a long one is pinned now
(below). Where a writer waits, the std lock holds new readers behind it on
macOS and Linux alike, so a read under the lock beside a busy writer waits
as before.

**A long read is pinned, and read with no lock held** (`engine/pinned.rs`,
`Database::pin`). Under the read lock a write waited for every read, so a
long one held them all: a ledger's reconciliation (a read-only `/batch`
of three aggregates over a million entries, 140-200 ms) held transfers up
to 187-512 ms, 36 past 50 ms in ten seconds (`make recon-bench`), and an
aggregate of 50 000 groups over a million events four writers' puts --
and every read by id behind a waiting one -- for its 0.8 s (`make
concurrency-bench long`). `pin`, under the read lock, takes what a read
of the documents alone needs into a `Database` of its own: each
collection's schema and its store as `Store::pinned` takes it -- the
segments and the mapped file shared, the id index copied, 12 bytes a row,
the stretches left out (an image's, which a read never writes) -- and the
read runs on that with no lock held, the database as it stood at the pin.
Nothing a write changes in place is shared: an append to a segment the pin
holds copies it first (`Arc::make_mut`, 8 MB at most, once a pin), a
handover or a compact lets go of its own hold only, and a checkpoint or a
compact renames its file over the one the pin maps, which the mapping
keeps. The pin took 0.26 to 0.87 ms over a million rows, and the read as
long as it took under the lock (151.9 ms against 151.4, 664 against
668); the writes beside it waited at most 2.5-10.8 ms, what they wait
beside no long read (handovers, the segment's copy), against 118-854 ms,
and the reconciliation's transfers 5.4-29 ms and none past 50 in three
runs of four. The fourth met a 385 ms stall, of the kind both servers met
in the bench's first round, which runs no reconciliation (118-223 ms):
the journal's `@unique` index growing its table whole as it passed 1.8
million entries, and `--warm` building an index the bench's warm-up had
not read as the round began (both below). With those gone, three runs:
1.9-10.4 ms, none past 50. The indexes are
not taken: a hash index's map, an ordered index's chunks, a text index's
postings change in place, and a copy under the lock costs what the read
spares -- a shared one, copied by the first write to touch it, would have
that write wait instead, a ledger's `@unique` over its entries a million
keys -- while a read an index answers is short anyway. So `pin` declines a read an index would answer --
an equality or `in` over a `@hash` field, a range or a one-key `order`
over a `@sorted` one, a facet the buckets or the ranges count, an inner
`get` on a hashed field, `expired()`, `match`, `near`, `lookup` -- and one
reading fewer than `PIN_AT` (10 000) rows by its shape (a page with no
`order`, a read by id, a `count` of everything, a statement alone let go
before its filter is walked), which run under the lock as before; every
index of a pinned collection is there by name and refuses (`PINNED_OUT`),
so a read let through by mistake is answered `None` by `Pinned` and run
again under the lock, the whole batch, never planned without the index
(`a_read_that_reaches_for_an_index_is_answered_none`). Readers taking the
lock in slices were not an option: a slice's end lets a write change the
answer. Copy-on-write indexes and a chunked id index would make the pin
O(chunks) and let an index read be pinned, for a pointer more on every
lookup and a copy of a chunk on the first write to each a pin holds; the
pin's copy is a hundredth of the scan it spares, so it stays the flat
copy. A database's pins hold `PIN_BUDGET` (256 MB) of id indexes at most,
past which a read runs under the lock. The server pins a lone read
(`/query`, its JSON path too) and a read-only `/batch`, and asks every
read: a short one declined costs 25 to 40 ns, a read by id 573 in process
and 17 to 24 us over HTTP (`make requests-bench` the same in turns); `tests/pinned.rs`
holds every pinned answer to the lock's at the pin, past deletes, updates
of indexed fields, an alter, a drop, a compact under the lock and beside,
a checkpoint, a handover a write, and three writers and a compactor
beside three readers. The browser module has one thread and none of it:
not a byte of it changed.

**Every write is a block, and a block is one record.** `execute_with` runs a
write as a block of one (`Database::execute_block` runs several, `begin`,
`commit` and `rollback` hold one open): `wal` and `note` hold its frames and
ids back (`Block`), and `commit` appends them as one record -- within one
collection a data record of as many frames, which a binary from before
blocks reads, across collections a `REC_BLOCK` (kind 9) of one data record a
write -- and only then notes them, so a crash, a replica and a subscriber
see all of it or none. A block that fails is undone in memory: each id it
wrote is pointed back to where its record was (`Store::point`) and
`Store::rewind` cuts each store back to its `Mark`, the documents it wrote
are unindexed and the versions before them indexed again, and the ids it
handed out are handed out again. A record is numbered by its last write, `writes_in` counts them, and
whatever counts records -- the replication feed, the archive,
`apply_records` -- counts that way; a restore to a change inside a block
stops before it. A compact cannot be undone, so it is refused in a block
(`Statement::fits_block`), and a `/batch` holding one runs each statement
on its own. A schema change is undone as a write is: the
block's log (`Undo`) holds a collection made, which goes, a collection
dropped -- kept whole until the block lands, then let go -- which comes
back where it stood, and an index built, which goes. It lands inside the
block's record (kind 9) as the record it is on its own, and a lone one as
that record alone, so only a block mixing one with other writes is new to
a binary from before 10.4, which refuses it as corrupt. Undone in one pass
the last write first, the collection a write is of is looked up by id, not
kept: a drop and a create of the same name in one block are two
collections. It cost the browser module 3.0 KB, 0.6 KB brotli, the
removal of an index from each of the five maps most of it; draining the
log rather than popping it was 0.5 KB more. A `/batch`, a keyed write
and the browser module's `run` of several are one block. A block is put back a write at
a time, the last first, each the inverse of what it did: 3.0 ms for 50 000
writes, where finding each id's first write by searching took 402 ms, and
a sort was 4.6 KB of the browser module. The buffers are kept from one block to the next (`Block::cleared`),
the record's header written into room left before the frames: allocated
anew they took a lone `put` from 832 to 985 ns; kept, a put costs 841
against the 829 before blocks, a `del` 648 against 634.

**A storage error stops writes.** Once the sink refuses an append, a sync or a
rewrite, every later write and sync returns `Error::Io` until the file is
reopened (`Database::failure`); reads go on, from memory and from the
mapped file's pages. A page that cannot be read in is the process's end
(`SIGBUS`), not an error -- which is what `--no-mmap` is for on a disk that
fails reads or a network file system -- and a mapped file is only ever
replaced by rename: a copy over it in place took a server down. A failed `fsync` is
never retried -- the kernel may already have dropped the pages -- and
`fenec-server` under `--sync always` reports it (500) instead of the success it
had not yet sent. That fsync runs *outside* the exclusive lock: under it a
write only calls `Database::flush`, which hands back a `Durability` to run once
the lock is released, and `FileSink` writes the bytes there as well (a `write`
under the lock waited out concurrent fsyncs on macOS). The writes the lock
still makes -- the 1 MB buffer's once it is full, a handover's -- take the
sink's disk, which a durability holds through its fsync, so both are put
off while one does (`Sink::syncing`, a `try_lock`): the buffer up to
`WRITE_HELD` (8 MB), a handover to twice its bounds. A ledger's transfer
landing a handover waited out the syncer's `F_FULLFSYNC` of a quarter
second's writes, 52 ms, every request behind it. A durability whose bytes
an earlier fsync already covered runs none, which is the group commit: 268 ->
1 156 durable writes/s over eight clients. A failed one is reported back with
`Database::fail` so the engine stops taking writes. The syncer of `--sync
<ms>` flushes the same way, a database's and each of a node's tenants'
(`Tenants::sync_dirty`): the tenants' held their lock through the fsync, and
every read and write of one waited up to 27 ms a pass on macOS.

**A durable write syncs a log beside the file, not the file**
(`synclog.rs`, Linux and Android). The file only grows, and on ext4 an
fsync of a file whose length changed commits the journal too: in Docker's
VM a 300-byte append and its `fdatasync` took 356 us, the same bytes
written into room the file already had and synced 65 -- why PostgreSQL
fills its WAL segments first. So `FileSink`'s sync writes what the file
took since the last one into `<file>.sync` (the whole name and `.sync`:
with the extension replaced, `x.db` and `x.fenec` shared one, as they
shared `.compacting` and `.beside` -- `fs::beside` names all three), 4 KB
of header and 256 KB of entries written once at full size and never
grown, and syncs that alone (`Tail`). The file is fsynced when an entry
would not fit, a sync holds more than 64 KB, or the sink is new to the
file -- whose bytes an earlier process may have left unsynced -- and the
log then begins a generation from where the file stands. An entry is the
file's bytes at a place, with its generation and a hash, so a cut one
ends the log; the header names the generation, where the file stood
synced, the file's device and inode, and a hash of the 4 KB before that
point, so a log is applied to its own file alone -- not to one renamed
into the place, even one holding the same bytes. `create` applies it
before the file is read or mapped (`synclog::recover`), fsyncs the file
and wipes the header, so a file cut below the log's end after that open
is not written over at the next. Everything that renames a file into a
database's place removes the log first and makes the removal durable --
`swap_in` after syncing the file it replaces, a restore, a backup
unsealed, a tenant's import (`fs::forget_sync_log`); a clean close syncs
the file and removes it, so a node keeps none beside a closed tenant, and
`Tenants::stats` counts it; a log that cannot be made is not tried again
until the next open, each try being 260 KB of writes. `open_read_only`
applies none: after a power loss, `fenec types` or a raw copy taken
before the next open may lack the last durable writes. On macOS
`F_FULLFSYNC` flushes the drive either way, 3.9 ms appended or not, so
there is no log (`ENABLED`; `fs::keep_sync_log` has a test thread keep
one). In Docker a durable update went 624 -> 325 us p50 (`make
roundtrip-bench`), against PostgreSQL's 378; `tests/synclog.rs` crashes
the machine (the process forgotten, the file cut back to the log's
start) through a full log, a sync past an entry, a torn header, a file
renamed in, a second open and two files of one stem, and fenec-core's
and fenec-http's tests pass on Linux, where every file keeps one.

**Replication ships only what is on disk, numbered by the change counter.**
A primary (`--replication-token`) writes through a `Tee`
(`fenec-http/src/replication.rs`): a write's record enters a bounded feed in
memory as it is appended, and is sent once an fsync has covered it -- so a
primary back from a crash holds every write any replica was sent. A replica
applies them with `Database::apply`, which does the write path's index upkeep
and hands each record on to its own sink with `Sink::record(seq, ..)`: its
change counter matches the primary's write for write, and its file reopens
where it stopped. So every write goes through `wal`, and a record moves the
counter by the writes it holds (a block's, `writes_in`); a record that moved
no counter would leave every replica one change off. The
history (record kind 8, `History`) moves none, as a graph a server keeps in
the tail does, and neither is sent. A promotion forks the history, a replica is continued only from a position
the primary's history passed through and sent an image otherwise, and a
following database refuses writes (`Error::ReadOnly`, 403). A durable
write is answered once every stream has written it into its socket
(`Feed::wait_sent` in the `Tee`'s durability, `SENT_WAIT` a second at
most, a stream that missed it `lagging` and not waited for until it
catches up): sent after the answer, a write answered in the moment before
was lost when the primary died -- a node of the SaaS example killed under
eight writers lost one in 3 of 60 runs on main, none in 120 since -- and
a socket's bytes reach the replica after the process is killed, though
not after the machine dies. Lag is 0.04 ms p50 under `--sync always`
(0.14 before the wait) and at most 283 ms under `--sync 250`, durable
writes 254 a second against 246; ten failovers under `always` lost no
acknowledged write. An archive (`fenec archive`,
`fenec-http/src/archive.rs`) is the same stream written to files, each write
with the time the primary appended it; `fenec restore` is an image plus the
archived writes up to a time or a change, forked -- a fenecdb file is exactly
that, so a restore is a concatenation checked by opening it. An archive
takes images of its own end (`Archive::consolidate`, `--image-every`, an
hour): it kept its first image for good, and a restore replayed every write
since. The segment being written ends at the next write after one, so
`prune` (`--keep`) lets go of whole segments -- in the open segment the
writes before an image stayed until it reached 64 MB. `fenec verify` reads
an archive as a restore would. A segment is closed and fsynced before the
next begins and an image renamed into place, so a sync tool's copy, taken
a file at a time while the archive writes, is the archive up to a moment:
fenecdb speaks no TLS, and S3 and R2 are reached with `rclone sync`.

**Scaling out is by tenant, one file each** (`fenec-server --dir`, `fenec-shard`,
whose directory replicates to a standby router like any other file, the
standby's maps catching up from the change ring -- 4 us a change at 100 000
tenants against 38 ms reading them all again under the router's write lock;
`site/content/docs/sharding.html`). The tenant comes from the path
(`/t/<tenant>/`, looked up again per request, so a move, an idle close or
a delete between two of them is seen), never from the query, so tenants
cannot share a file -- they
would read each other's rows. A file each also keeps ids, the change sequence,
BM25 statistics and `lookup` per tenant, which is why the router forwards bytes
and never parses a query. The registry (`fenec-http/src/tenants.rs`) opens a
file only under its lock after checking the open map, and closes one only when
the map holds the last `Arc` -- a second `Database` over one file corrupts it
exactly as a second process would. `freeze` takes a per-tenant gate
exclusively so a write that passed the frozen check cannot land after the final
export. A move is freeze, copy the image, install, flip the directory in one
statement, delete the source; the change sequence travels in the image, so a
caught-up subscriber resumes on the target without a reseed. A node's tenants
are replicated to its standby, each through its own feed at
`/t/<tenant>/_replication`, and `POST /_shard/nodes/<n>/failover` promotes
them there one at a time -- separate databases, nothing to make atomic
between them. A tenant's role is its file's, not the node's `--replica-of`:
a replica's file follows, a primary's stays one, and a file new to a replica
node follows (the standby copy the router made). Every file on a replica
node was made to follow once, so an idle close or a restart undid a
promotion and the old primary's image wiped the writes since. A rejoining
node's tenants follow when the router records the pair
(`POST /_admin/tenants/<t>/follow`, each), a failover that moved every
tenant ends the pair, and no tenant is placed on or moved to a standby. A
tenant whose follower runs is never closed as idle: the follower holds the
database rather than the tenant, and a close left it writing the file under
the next instance. The router cannot tell a node that is gone from one it cannot reach,
and guessing makes two primaries, so it promotes on its own only under a
lease (`fenec-shard --auto-failover`, nodes `fenec-server --dir --lease`;
`fenec-shard/src/lease.rs`, `fenec-http/src/lease.rs`): a node stops
writing as its lease lapses by its own clock -- measured from when the grant
came in, where the router measures from when the answer came back, and
waits a tenth more -- and only then are its tenants promoted. The fence is
the engine's (`Database::set_fence`), asked in `commit` as a block lands and
in `may_write` before a schema change or a maintenance, so every write path
honours it; asked as a lone write started too, it took a `put` from 827 to
898 ns, once 822 to 846. The lease names
the node's primaries, not the node: a node back from a failover writes none
of its stale copies before the repair its answer triggers has them follow.
The list rides only when its epoch (FNV of the sorted names) changed, and a
node that does not hold it answers 412; a create or a move grants the new
list at once. Grants go out in parallel over a pool bounded by a third of
the lease, or one silent node let the others' lapse. A standby router forgets
its leases while it follows and counts from its promotion; a node without
`--lease` is never failed over on its own. A lease of a second: a node cut
off stopped writing 697 ms after, its 10 tenants took writes again 1.29 to
1.33 s after. A
write is on the standby 0.089 ms after the primary answered it (p99 0.448),
and 20 tenants failed over in 60 ms (`make shard-bench`). A standby waits
idle and takes all of a node at once, so with `fenec-shard --replicas` a
tenant on a node in no pair gets a replica of its own instead (the
`replicas` collection): on the node holding the fewest replicas of its node,
then the fewest in all -- the fewest in all alone had every tenant of a node
follow on the same other, ties going by name. The node is told
(`POST /_admin/tenants/<t>/follow {from}`) and keeps the URL in
`<t>.follows` beside the file for its restarts (`resume_following`); a file
made to follow ends what it served first (`release`, as a delete does), or
the old primary's feed held it. A failover of a node in no pair promotes each
tenant on its own replica, `POST /_shard/replicas` gives replicas back -- on
a node holding a copy first, so a returning node's old primaries follow --
and a move re-points the replica or, onto its node, places another. Three
nodes, 10 tenants each: one node's failover took 57 ms, 5 onto each other.

**A scoped token is held to its rules at every level, twice for writes**
(`fenec-http/src/access.rs`). A JWT's policy filter is ANDed into the statement
-- the `where`, each `lookup` level, a subscription's shape -- so a new path
that runs a statement must go through `scoped()` or it reads everything. Writes
also get `WITH CHECK`: the `Check` write hook tests every document a put or a
set writes against the filter, found through a thread-local set by `within()`
around the execution -- a scoped write executed outside `within` goes
unchecked. A scoped subscription keeps the ids it sent and reports deletions
only for those; the unscoped shape's "a changed id that does not match is a
deletion" would hand every user everyone's ids. A scoped stream of a shape
that holds nothing (`where=false`, what `FenecHttp.live` opens) follows the
token's own filter instead, its ids kept server side and never sent, and a
write to one, or one leaving, is a change naming no row: ANDed with the
filter the shape matched nothing and a scoped live query never ran again,
and told of every write it would have had the moments of other users'. A
stream is ended at its token's `exp` (`Scope::expires_ms`, the keep-alive
wait bounded by it) with `event: error` and `{"error":"the token has
expired","status":401}`, which every client takes as a refused request --
`FenecHttp.live` and `subscribe` stop with a `FenecError` of status 401,
the sync layer asks for a token (a scenario), Go's stream ends with an
`*Error` of 401, .NET's with `Event.Status`: a stream opened two seconds
before `exp` delivered a change written four after, while the token's `get`
was 401. A scoped read's poll tag is a digest of its answer (`content_tag`,
the query run every time): the change counter's tag, and a 304 answered
unrun, told it when anyone wrote to what it reads. The algorithm is the key's,
never the token's: `--jwt-keys` reads a JWKS file, `oct` keys for HS256 and
`RSA` keys (2048 bits up, `crypto::RsaKey`, 64-bit limbs: 166 us a check in
32-bit ones, 38 now) for an identity provider's RS256, a `kid` picking the
key; an RSA modulus taken for an HS256 secret would sign anything. The file
is read again as it changes, looked at once a second, which is the rotation.
A verified token is kept by its text (`verified`, 16 shards of 256, emptied
when the keys change, `exp` asked each time): 0.27 us against 2.9 for HS256.
A token naming no `exp` is refused (`Demands::require_exp`,
`--jwt-require-exp off` takes it), and `--jwt-max-age` bounds how far ahead
one may lie; `mint` stamps an hour on claims naming none. Grants are
`read`, `insert`, `update` and `delete` (`write` the three, bits in
`Rule::ops`), `update(a, b)` the update of those fields alone
(`Rule::fields`): `update` on `accounts` let a ledger's app token `set
accounts {balance: balance + 500000}` and change an account's kind too,
and `WITH CHECK` saw only the row after. A write over a row is judged by
the fields that differ, as encodings (`Scope::overwrites`, through the
hook's `before_overwrite`, which the engine calls with the row as it was
-- a `set`'s and a `put`'s over an id, the old row read before the hooks
rather than after, so no read more): each must be one granting rule's,
that rule admitting the row before and after; a policy naming no field
list judges nothing there, and an unscoped write meets an empty hook
list or a thread-local found empty. And the `Check` hook takes the write's op: an insert held to
the rules granting it, an update found and checked by its own, a delete
found by its own; an update and a delete need read beside them, an insert
none, so a client appends to a stream it cannot see. `<c> append-only`, a
line of no filter and no role, takes updates and deletes from every
scoped token whatever its rules say -- a rule naming them there is refused
at startup -- and not from the server's token, which keeps corrections,
erasure and `@ttl`'s sweep: in the schema it would bind the operator too,
a format change for an invariant whoever holds the file can break anyway.
A list claim goes after `in` (`room in $jwt.rooms` is read as `in
[$jwt.rooms]`, which `bind` spreads), more than `MAX_CLAIM_VALUES` (1 000)
refuses the token, and claims are read exact (`json::parse_json`): a list
of numbers read the quick way was `f32`s, team 123456789 matching
123456792. A `match` the token reads through a filter is `Match::within`:
BM25's count, mean length and document frequencies over the rows its
filter selects (`TextIndex::search_within`), so a score is the one a
collection of those rows alone gives -- alice's memo went 9.87 -> 4.79
once bob wrote 200 holding the word -- 0.16 -> 0.24 ms for 2.5% of 100
000 rows; the browser module leaves the counting out.

**A token is bound to the tenant it names** (`Scope::reaches`,
`route_tenant`). The policy is the node's, not a tenant's, so `owner =
$jwt.sub` matched alice's rows in every tenant's file: a token minted for
one tenant read and subscribed to every other, on its node and through the
router. On a `--dir` node a JWT reaches `/t/<t>/` only when its tenant
claim (`--jwt-tenant-claim`, `tenant`) names `<t>`, a text or a list
holding it; one naming none is 403 unless `--jwt-unbound-tenants`. It is
asked once in `route_tenant`, before the tenant is looked up -- so a
refusal says nothing of which tenants exist -- and every route below the
prefix passes there: a new one cannot skip it. The router forwards
`Authorization` and holds no keys, so the node enforces it. A tenant is
created with its schema through the router (`PUT /_shard/tenants/<t>
{schema}`, FenecQL text read before anything is placed, applied on the
node by `POST /_admin/tenants/<t>/schema` with the admin token the router
holds, once the lease names the tenant, and the tenant taken back off the
node and the directory when refused): applying it took the nodes'
`--http-token`, which reaches every tenant, so the app creating tenants
held it. A tenant's
database installs the `Check` hook as a single one does
(`Tenants::check_scoped_writes`, from `Server::with_tenants`): none did,
and a scoped write to a tenant went unchecked.

**fenec studio is a page with no authority of its own** (`studio/`,
`fenec_http::studio`, `--studio` on `fenec-server` and `fenec-shard`; off,
`/_studio/` is a 404 as an unknown path is). Plain ES modules and CSS with
no build step, importing `web/client.js` as any page would; the files are
embedded by `include_bytes!` under fenec-http's `studio` feature, which the
two binaries turn on and the shell does not: +210 KB of each release binary
on aarch64-apple-darwin, the files 193 KB of it, and the later views 116
KB more of `fenec-server` and 99 of `fenec-shard`, their files 106 KB.
They are served before any token is asked, with a strict CSP (`script-src 'self'`, no inline script,
`connect-src 'self'` and the one `--studio-connect` origin, which is
checked to be an origin since it is written into a header), `DENY` framing,
the page `no-store` and the rest revalidated by an `ETag` of their bytes;
the names are not hashed, since the modules import each other by name. The
token is pasted into the page, kept in its tab's `sessionStorage` and sent
as a client sends it, so the server's scope rewrite holds the grid, the
counts and the facets to a scoped token's rows. Every statement is written
in `studio/statements.js` -- the later views' in `statements-views.js`,
under the same rules: values as parameters, a name refused unless it is
one FenecQL writes, the typed `where` held inside parentheses it cannot
close; writes are `/batch`es under an `Idempotency-Key`, a cell's `set ...
where id = $2 require 1`, so a row deleted meanwhile is a 412. `GET
/_whoami` is what it shows a token by: `open`, `full` or `scoped` with the
rules that apply to it, the filter as the policy writes it
(`Scope::summary_into`). The grid reads 100 rows a block, the block after
one in id order by `id > $last`, a jump or an order by offset, and draws
only the rows in view: a scroll down 100 000 rows and back took no long task,
the longest frame 18.7 ms in headless Chrome. Its first load is 60 KB of JS
and CSS gzipped (`site/build.py` holds the docs to it), served uncompressed.
The query editor, the schema, the live rows and the admin view are modules
fetched the first time each opens (`app.js`'s `LOAD`, their stylesheet
`views.css`): 19, 19, 7 and 13 KB gzipped with what they share, none of
it on the first load (`statements.test.mjs` holds both). The editor runs
the text as typed -- cut at each `;` outside strings and comments, one
statement to `/query`, several one `/batch` -- and shows the status,
`Fenec-Seq`, `X-Request-Id` (exposed to CORS for it) and `explain` of a
`get`; it is coloured by `studio/highlight.js`, `site/highlight.js` with
build.py's rules written in (`make studio-highlight`;
`site/test_highlight.py` refuses a stale copy), whose mirror is text
nodes, never `innerHTML`. The schema view shows a collection as the
engine writes it (`GET /_schema?as=fenecql`) and every change as its
statement and `/_schema/plan` of the collection as it would be; a drop
runs once its name is typed, and a text box's `change` on blur no longer
asks the plan again, which held the apply button back. The live view is a
subscription of the typed shape, refused past 10 000 rows since its seed
holds them all; the admin view, for the server's token, reads
`/_stats/statements`, `/_metrics` and on a router `/_shard/tenants?bytes`
(each node's `/_admin/sizes`). The site's playground is the same studio
over a database in the page: every view reads through `state.db`, one
surface -- `run`, `batch` and `subscribe` as `FenecHttp` has them,
`request(path)` for an answer shown whole, `stats()` -- which
`connect.js`'s `Remote` answers over HTTP (its `request` the views'
`kit.js` code, fetched with them) and `local.js`'s `Local` from the
browser module in a dedicated worker (`worker.js`), as a server would
(`engine.js`): `/query`, `/batch` as one block over calls
(`Fenec.batch`, the module's `fenec_block`), `/_schema` and
`?as=fenecql` (`Fenec.describe`, `fenec_schema` modes 3 and 4),
`/_schema/plan` (the JSON description written as FenecQL, which alone
the module reads), every refusal with the status `api::status_of` gives
-- a parse's message kept whole, as a server keeps it -- and a
subscription as a `Fenec.live` of the shape whose differences are the
change. The token, the tenant, the admin view and the file's sizes are
left out rather than faked, "Running in your browser." where the token
was, with Keep it in this browser (`persist`, off by default) and Reset
data. `studio/test/transport.test.mjs` sends the same statements, batches,
schema requests and a subscription's writes to a debug fenec-server and to
the module and holds every answer equal; a server's change also names as
deleted a written row its shape never held, which a view passes over, and
the test drops those. `site/build.py` serves the studio's own files in
`dist/studio/`, minified and named by their hash with every import
rewritten, the page `studio/index.html` with `data-mode="local"`, which
`app.js` takes to wait for `site/playground.js` to call `local(db,
{examples, first})`, and `local.css`; the playground frames it between the
site's header and footer, since both stylesheets name `.top`, `.btn` and
`--sun`, and `_headers` lets the site alone frame `/studio/*`. A
server's first load names none of it (`statements.test.mjs`) and grew
1.1 KB gzipped for `Remote` and the branches; the module 3.4 KB, 1.0 KB
brotli, `describe` 1.5 KB of it. The playground's studio is 72 KB of JS
and CSS gzipped beside the module's 200 KB brotli, its first answer
150 ms after the page is asked for with the cache off on this machine and
1.6 s over a throttled 4G, Lighthouse 98, 100, 100 on a phone.
`make studio-test` runs it in Chrome (puppeteer-core, `studio/`'s one dev
dependency) against a debug server, a tenant node and a router; on macOS
Chrome 148's new headless mode never answered puppeteer's clicks, so the
harness prefers the headless shell, and it builds the site and drives the
playground as well (`local.test.mjs`). The router run's one timeout did not
come back in twelve runs at a load of 27 (busy loops on every core, three
suites at once); a run filtered by name charges its first test with the
`before` hook's 100 000-row seed, about 10 s, which is the hook's and not
the router's.

**A server's `create index` and `compact` run beside the database**
(`Database::maintain`, `engine/maintenance.rs`): what the build reads is copied
under the read lock, the build holds no lock, and the write lock is taken only
to apply the writes made meanwhile and put the result in place. Those writes
are known exactly, not from the change ring a long build would overflow: every
write passes through `Database::note`, which hands the id to the `Tail` of each
maintenance on that collection -- so a write path that skipped `note` would
leave a built index missing it. A schema change there (another index, a drop)
fails the maintenance rather than installing what no longer fits. At 100 000 x
128 reads waited at most 21 ms through an HNSW build and 30 ms through a compact
(file rewrite included), against the full ~20 s under the write lock. Over a
mapped file a compact copies no record: the graphs holding tombstones are
rebuilt beside the database, then each store is cloned under the read lock --
the records in the old file and the sealed segments shared (`SegmentBytes` is
an `Arc` natively), the index copied, each graph's record written -- the live
records written into `<file>.fenec.beside` with no lock held and fsynced, and
the clones relocated onto it without reading it back (`relocate_live`). Under
the write lock only the documents written meanwhile go in, as a data record
inside the image, whose header then takes the counter as it stands, and the
side file is renamed into place (`Sink::adopt`, through a primary's `Tee`
too). What they superseded stays in the new file, dead, for the next compact.
At 1.8 million x 128, a 1.1 GB file, writes waited at most 24 ms against 1.22
s, for a peak of +67 MB; cloning the segments rather than sharing them held
the read lock 508 ms and took +1.16 GB. One side file at a time; a collection
created, dropped or altered meanwhile fails it. Only a lone statement takes
this path; a batch, the shell and `execute` hold the lock.

**A file compacts itself once half of it is dead** (`engine/garbage.rs`).
An update appends the whole record and leaves the version it replaced dead
until a `compact`, which nothing ran on its own: under YCSB's updates a
million 1 KB records grew a buffered file to 4.7-6.5 GB on an 8 GB machine,
a live record a few to a page among dead ones, and one read in five waited
for the disk (C at one thread 30.1k ops/s, 443k over the file freshly
loaded). `Database::garbage` is the file's bytes as written (`appended`)
against the live records' (`Store::total_bytes - dead_bytes`, which the
stores counted as writes landed already and an image's index carries) and
what the last compact kept besides the documents (`kept`: schemas,
counters, graphs, the file less every record the stores hold -- the
versions a compact beside the writes copied and saw written over are dead,
and counted as kept they left a 307 KB file of 53 KB of rows not due
again) -- a sum over the collections, nothing added to the write
path: a lone put, put over and del measured 570/700/532 ns in memory either
way, and 666/797/595 against 660/785/593 over a mapped file, inside the base
build's own spread. `CompactPolicy` is the one rule (half the file dead and
64 MB, `compact_due`), `compact_when_due` a look under the read lock and a
compact beside the database, `Compactor` a thread that looks every 5 s. The
look waits for a write under way: under `try_read` 10 looks in 12 runs of
`make compact-bench` found the lock taken and put a due compact off 5 s,
the file growing 100-160 MB a second meanwhile.
`fenec_http::link` runs one thread over every database the process serves
(`--auto-compact <ratio>|off`, default 0.5), apart from the graph keeper so
its graphs do not wait out a compact of a gigabyte; the native library
starts a `Compactor` a file (`FENEC_OPEN_NO_AUTO_COMPACT`); a replica and a
following tenant compact too, since `may_write` lets a compact run there and
their files grow with every update sent, and an image adopted during a
maintenance marks every tail changed (`Tails::all_changed`), or a replica's
compact put back what it copied before the image. The browser has no thread:
`snapshot` writes the live records alone, so a page's persisted image drops
them, and its memory keeps them until it reloads. The swap was the cost:
the writes made during a 4-6 s compact of a 1 GB file, 200 000 records, were
copied under the write lock (409-811 ms); they are copied in rounds before
it, the ids drained from the tails at once and their documents looked up
4 096 at a time under the read lock and copied after it (`Store::snapshot`:
a payload in the file by its place, which an append never moves, one in
memory copied out; 95-245 us held at the median, 3.1 ms at most) -- copied
under it, one round of 408 708 held the readers 592 ms behind the waiting
writer, and with its pages touched first a writer at 170-180k updates a
second ran at 1.5-37k through the worst 100 ms of a 1.2 GB file's compact,
83-144k now -- until a round finds fewer than 1 000. The rounds' frames are
handed over to the side file before the swap (`Beside::hand_over`; left to
the first write after it, 400 MB under the write lock, 158 ms), every page
of the new file is asked for 16 MB at a time ahead of its reads and read in
before it (`fs::touch`, `MADV_WILLNEED`: a fault at a time a 1.2 GB file
took 3.0-3.3 s on this 8 GB Mac, the writer beside it at 11-15k, and its
pages were out of memory again by the swap, the 2 s after it at 21-39k;
asked for, 0.6-0.8 s and 90-150k; left to the readers, the read p50 went 2
-> 100 us for two seconds), and the stores it replaces are dropped after the
lock (their unmapping was 10-15 ms of it). Under the lock: the last few ids,
the side file's fsync and the rename, 6.1-12.6 ms over a 1 GB file and
13.7-15.7 durable (`make compact-bench`: 5M updates of a field over a
million 1 KB records from one writer, buffered, the file 1.03 -> 2.1-3.0 ->
1.1-1.6 GB three or four times where it reached 6.2 GB without, and after
the updates 435k reads/s from the first second where the 6.2 GB file
started at 19k and took 25 s to 400k). What a compact still costs that
writer is the page cache's, not its own: beside this Mac's apps the cache
holds about 2 GB of the file, the size at which a compact comes due, and
past it the updates read most rows from the disk whether one runs or not --
28-46k a second at 2.25-2.75 GB with the compactor off, 24-32k while a
compact writes its image. Over nine runs each in turns, the median 100 ms
of a compact went 27.1k -> 30.6k, its worst 1.1k -> 3.6k, a compact 10.9 ->
7.8 s, the reads during it p99 2.3-6.5 -> 1.3-5.3 ms; at 250 000 records,
whose file fits, 143k -> 153k, the worst 16.9k -> 129k against 180k with
none, a compact 1.0 -> 0.3 s, the reads p99 0.5-1.3 -> 0.14-0.35 ms and 52
-> 18 ms at most. Paced to half and to a quarter of its speed a compact
left the writer where it was (30-35k) and took 56 and 95 s against 10; at
utility QoS its reads were throttled behind the writer's and it took 138
s; with its side file uncached (`F_NOCACHE`), or at utility QoS for the CPU
alone, nothing moved. A database read into
memory compacts beside itself too, the live records copied into fresh
segments with no lock held; it used to copy them under the read lock and
build every index again, though the indexes are keyed by id.

**File format** (see README *File format*): every record is
`[kind][collection-id][length][body]`. The length is written even for an empty
body and the reader **must** consume it, or the stray byte is read as the next
record kind. A last record a crash cut short is cut off the file on open
(`Database::load` says where, `fs::open` and `replication::open` cut): only
skipped, it swallowed the next append, an acknowledged write lost on the open
after. Only past the checkpoint image, though: an image is renamed into place
whole, so a record cut short inside it -- or an image longer than the file --
is refused as corrupt; cut there as a torn tail is, one flipped bit deleted
every record after it. A tool that only looks (`fenec types`) opens with
`fs::open_read_only`, which cuts, creates and writes nothing: a server's
append in flight looks torn from outside. The change counter record (kind 6) is at the front and fixed width;
the id counter (kind 7) exists so `compact` cannot hand out a deleted id again,
and in an image carries behind the counter the index of the data record right
after it (`Store::image_index`) -- each live document's id, payload length and
place, 4 bytes a document -- which a mapped open takes (`adopt_index`) rather
than walking the frames' heads, a chain of cache misses: 100 000 x 128 opened
in 8.4 ms against 14.4, and every reader of the record reads the counter
alone. Read into memory, a load copies each frame as it stands into the
segment `append` would put it in (`replay_noting`): framed afresh into a `Vec`
of its own first and copied again, it was 17 of a 25 ms load, now 9.6;
the history (kind 8) and a graph a server keeps in the tail (kind 4) are the
appended records that are not writes; a block (kind 9) holds a record for
each of its writes: a data record, or a create's, a drop's, an index's or
an alter's (kind 12).
A head is written in one place (`head`, the counter's `image_head` aside)
and read in one (`record_at`), and a walk over a file's records is a
`Walk` -- a load, a repoint and the search for the last graphs each wrote
their own once -- which steps over the counter's fixed-width head and
stops at a record cut short for its caller to judge.

**The HNSW graph is derived data, not a cache.** It is written by
`snapshot`, `compact` and `checkpoint`, and by a server into its file's tail
(below) — never on the write path. On open the
version, dimension, precision and link bounds are validated, and the live nodes
against the documents holding a vector -- one each, no two of one document,
and counted by reading every document's field only when the nodes are fewer
than the documents: the scan was 5.4 ms of a 36 ms open at 100 000 x 128,
where every document held one. Anything off means a silent full
rebuild. A corrupt graph can therefore never lose data. It is restored where
its last record is -- a checkpoint's image holds one after each collection's
data; `last_graphs` walks the record heads for it first, since restoring each
record read every vector again -- against the documents as they stood there,
and the writes after it are applied as the write path would (a touched
document keeps its node while it holds the same vector, and has it retired for
the new one otherwise) -- restored after the whole file, one write in the
tail threw it away, and a crash cost 48 s at 100 000 x 768 instead of 0.99.
An index added in the tail (`create index`) leaves the graphs restored
before it, as a replica leaves them; reset with the rest, it had the next
open build them all again. A
tombstone carries its own vector in the record, since its document may be gone:
without that, one `del` rebuilt the graph on every open until `compact`.
`compact` rebuilds a graph holding tombstones and leaves the rest: nothing
else takes one out, every write that changes or deletes a vector leaves one,
and they crowd the beam `near` walks. A rewrite touches only the indexes
whose field it changes (`unindex_doc` and `index_scalar` take both versions,
and `VectorIndex::insert` keeps a node that already holds the vector): an
update of a title took the vector out of the graph and back in, 1.89 ms at
20 000 x 768 and a tombstone, against 0.006 ms now. "Unchanged" is to the
bit, since `==` says -0.0 is 0.0 and a hash key is the value's encoding. An unfiltered `near` they cut short walks
again with the beam wider by their number -- no more than all of them can be
in it -- or searches exactly where that walk costs more than reading every
vector (`past_tombstones`); without it a `limit 10` answered 4 rows.

**A restore gives back the arena it had, to the bit.** Restoring a graph
reads each live node's vector out of its document and normalises it as the
write path did, the flat sum's order and all (`flat_sq`: the 8-strip `norm`
rounds otherwise, and the graph would be another). One add waiting on the
one before made that a third of an open, so natively eight vectors are
summed side by side, each in its own order (`flat_sqs8`), and the links --
two or three bytes each -- are decoded inline (`graph_varint`), their list
buffer kept: a checkpointed 100 000 x 128 file opened in 29.8 ms against
41.0, 100 000 x 768 in 70 against 134, and
`a_restored_arena_holds_the_vectors_it_had_bit_for_bit` holds the arena to
the one it restored. The browser loads a 20 000 x 128 image 13% faster from
the rest; summing side by side gained it nothing there, and would have cost
0.5 KB brotli. Natively the arena is filled a share of `FILL_SHARE` nodes at
a time by whichever thread is free, each writing its nodes' slots where they
stand (`fill_restored`) rather than reading a vector, copying it into a
batch and pushing it: 100 000 x 128 opens in 16.5 ms against 21.8, x 768 in
42 against 61, any share from 256 to 4 096 nodes the same. The mapped
file's pages it reads are most of what is left of that; `MADV_WILLNEED` took
off 3%. The browser fills in turn: the shares were 1.3 KB brotli of its
module for the same load. Each document's node is a `DocMap` (`store.rs`),
a dense array where the ids are, as the store's id index, which the text
index's lengths share: filled as a `HashMap` it was 2.1 of a 16.7 ms open.

**A graph record is laid out flat.** Versions 7, an image's, and 8, a
server's tail, hold the nodes' documents, flags, levels and level-0 lengths
as arrays, then every list sorted, its first link in as many bytes as the
node count needs and its steps in as many as its widest (`put_links`).
Decoding a varint a link was a quarter of an open; written 4 bytes a link
and 8 a document, a 100 000 x 128 graph took 12.4 MB against the varints'
6.2, and a file 5% more. Laid out so, that file opens in 35.9 ms against
43.5 at 58.97 MB against 58.49, and 100 000 x 768 in 63.6 against 67.5.
Both layouts are read into the same arrays (`Read`) and the index is built
from them in one place (`build_restored`);
`a_flat_graph_record_restores_as_the_varint_one_did` holds 3 to 6 and 7 to
8 to the same index, which a binary before 7 builds again. The browser
module carries the reader -- 1.8 KB brotli -- to open a server's file
without building its graphs again, and loads its own no faster for it.

**A batch links in parallel, into the graph it would link in turn.**
`insert_batch` finds a batch's neighbours on every core against the graph
before it (`compute_candidates`); `link_batch` then sets each node's own
lists and groups what they add to their neighbours' lists by list, each
group applied in node order on whichever thread is free (`spread`). A
pruning reads only the list it prunes and the vectors, so every list ends
as linking the nodes one after another left it, and
`a_batch_links_as_its_nodes_would_one_after_another` holds the graph
records to that byte for byte over every arena. Linked on one core, the
pruning was 70% of a 100 000 x 128 build: 10.6 s then, 5.3 now; 48.9 ->
23.4 s at 768 dimensions, and 50.6 -> 22.0 s for a crashed file's vectors
linked at the open. A share of the work a thread left the M1's efficiency
cores finishing last while the rest waited, so the work goes out an item
at a time. The browser has one thread and links in turn (`link_node`),
through the same pruning (`GraphView::pruned`).

**The upper layers are walked with a beam.** A search and a node joining
the graph walk each layer above level 0 with a beam of four (`UPPER_BEAM`,
`descend_beam`) and start level 0 from all four, where the greedy descent
took one node at a time. Over a million 128-dim vectors in 64 clusters it
left 20 of 1 000 queries in another cluster, and level 0 has too few links
across clusters for any beam to cross back: 12 found none of their ten,
and recall stopped at 97.4% from a beam of 200 up to 800. With the beam,
92.5, 99.1 and 99.8% at 40, 100 and 200 against 89.9, 96.4 and 97.4 --
pgvector's at the same m and ef_construction 93.0, 97.4 and 97.8 -- for a build
3 to 7% longer and a search as fast (`make scale-bench`). Both ways are
needed: in the search alone it gave 98.2% at 100, in the build alone a
graph the greedy search lost 32 queries in. The browser module grew 150
bytes brotli.

**`make scale-bench` holds fenec-server to pgvector, each over its own
protocol.** fenec-server is asked over HTTP by a keep-alive client written
in the bench crate (`fenec-bench/src/http.rs`), PostgreSQL by the
`postgres` crate with pgvector-rust's types, with the same vectors and m and
ef_construction, and each side takes its best way: fenec-server keeps its
graph as JSON arrays of rows land (`POST /<collection>`), pgvector builds
after a binary COPY in memory on every core (`parallel_workers`: 768-dim
vectors live in TOAST, and PostgreSQL planned one process for them), reads
its index into its buffers and searches a filter with `iterative_scan`; the
container needs 2 GB of shared memory for that build (`pgvector-up`).
At a million 128-dim vectors fenec-server loads and indexes in 49.6 s against
108.5, on 612 MB of disk against 1 432, its engine counting 738 MB against
PostgreSQL's 1 131 (its container's anonymous memory and the shared memory
of its buffers, from its cgroup; macOS puts fenec-server's physical
footprint at 992), answers a beam of 100 with 99.1% recall in 0.141 ms
against 98.6% in 2.08, a filter keeping 1% in 0.67 ms against 16.1, and
eight clients at 18 852 queries/s against 2 289; at 250 000 x 768, 39.9 s
against 79.9 and 99.8% in 0.431 ms against 98.1% in 4.22 (`site/content/docs/benchmarks.html#scale`). The laptop this was
measured on is a fanless M1 Air, which slows to a third under minutes of
load on every core: a comparison runs its sides in turns (`--only fenec`,
then `--only pg`), each after idle minutes, and a figure from a hot run is
not one.

**`make ycsb` is fenecdb as a general database.** YCSB's core workloads
A-F written in the bench crate (`fenec-bench/src/bin/ycsb.rs`) rather than
run through the Java YCSB, to its definitions: ten fields of 100 random
characters, a read the whole record, an update one field, its scrambled
zipfian (0.99, FNV-hashed over ten billion items), its skewed-latest for
D, scans of 1 to 100 records for E. The key is an integer from 1
(`insertorder=ordered`, no `user` prefix), so each system keys by what it
keys best by: `id`, `INTEGER PRIMARY KEY`, a `bigint` primary key,
`_id`; E's scan is `where id >= $1 limit n`, the id index walked from
the key, no `@sorted`. A key not yet acknowledged is never drawn: an
insert publishes a bound below its key before it takes one (`Keys`).
fenecdb in process against SQLite (a connection a thread, WAL, mapped),
fenec-server over HTTP against PostgreSQL 17 and MongoDB 8, each started
in a container for its turn and removed with its volume, so the VM holds
one at a time; `server-docker` is the same server in a container, which
shows Docker's share -- its network, and an fsync in the VM that is not
macOS's `F_FULLFSYNC`. Durable is an fsync a write in each engine's own
terms (a flush and its durability, `synchronous=FULL` with `fullfsync`,
`synchronous_commit=on`, `j: true`), buffered its default
(`--sync 250`, `NORMAL`, `off`, `j: false`); C writes nothing and runs
once. A server switching modes is stopped with SIGTERM, which syncs: a
kill lost the last 250 ms of the load, and the next update found no row.
Each cell is 30 s after a 5 s warm-up, and starts once a one-core probe
runs within 4% of its idle time (`cool`), the probe's ratio written with
the cell; the systems take turns with three idle minutes between them.
Latencies go into a log-linear histogram, 64 steps an octave.
`--verify` holds every answer to what was written (`check`): each
thread logs its operations and answers into its own `Vec`, and after the
cell a read must hold, field by field, a value a write that could have
been the last one left (one ended before it and not followed by another
that did, or one overlapping it), a scan consecutive keys as far as it
could see, a write one record -- every write logged, since a read under
concurrent writes is judged by every write's span, and nothing compared
in an operation's time. A pass over all six systems found no mismatch
(`ycsb/verify.tsv`). `ycsb/results.tsv` is the three runs the site
quotes, appended to and never rewritten -- a run's cell measured again
counts as its last line, and a run a later one replaced is named in
`ycsb/superseded.tsv` and counts in no median (`ycsb report`, `site/build.py`)
-- and `site/build.py` holds every YCSB figure on the site to its median
there, the full grid row by row. Docker's VM syncs a
write in 0.08-0.10 ms where the Mac's `F_FULLFSYNC` takes 3.9
(`ycsb/fsync.txt`), so the durable server comparison is `server-docker`
against PostgreSQL and MongoDB, all in the VM, the native server beside
them. fenecdb's buffered reads lost to SQLite's after the updates for the
engine's reason, not the harness's: an update writes the record again at
the file's end, the file outgrew the page cache, and a profile put 86% of
a one-thread read on its page coming in (`ycsb/profile-c1.txt`). Runs
`c1`-`c3` measured fenecdb and fenec-server again with the file compacting
itself (below; the harness starts the engine's `Compactor` as an app's
library does, `--no-auto-compact` for neither): C at one thread 30.1k ->
438k against SQLite's 279k, B 30.8k -> 410k, D 62.6k -> 432k, and fenecdb
ahead of SQLite in every in-process cell; the buffered file stood at 3.7-4.0
GB after D, between compacts of a file A grows by 100 MB a second. Official
YCSB 0.17.0 against the same containers came within -14% to +2% of the
harness (`ycsb/calibration/`). Docker's network moves from day to day --
PostgreSQL's C at one client was 3.34 k, 3.00 k and 3.53 k on three -- so
the one-client server cells were measured again on 2026-10-04 with
PostgreSQL beside them in turns, under the same run ids (`c1`-`c3`,
`r1`-`r3`), each cell counting as its run's last line. `make
roundtrip-bench` took the round trip apart: of a 0.29 ms read in Docker
about 0.25 is Docker's port forwarding, the server's part 0.023 ms and
PostgreSQL's bind and execute 0.016, so the published guess that a binary
protocol answers sooner was not the cause; the durable gap was the fsync
of a growing file (the sync log, above): durable A 2.24 k -> 3.27 k at one
client against PostgreSQL's 2.60 k, even at 16. E's scans were
fenec-server's own -- 213-220 us inside for 50 rows, 37 page faults --
and a plain `get` written as JSON from the stored rows (below) took E at
one client 1.03 k -> 1.62 k buffered against 1.34 k, 1.06 k -> 1.82 k
durable against 1.39 k. B, C, D and E were measured again on 2026-10-05,
servers and PostgreSQL in turns under the same ids, run 1 again on a quiet
machine after its idle probe was taken at a load of five; C stays 3.19 k
against 3.66 k, with each server's part of a read under 0.02 of 0.29 ms
and PostgreSQL's own B at 2.65 k beside its C: the rest moves with the VM.

**A block's `put`s link their vectors together.** A block
`Database::begin` opened -- a `/batch`, a keyed write, the browser module's
`run` of several -- is its statements' batch: a `put` in it leaves its vectors waiting
(`defer_batch`, `Block::waiting`), and they are linked on every core 512
at a time a field (`LINK_AT`, `link_waiting`), the rest as the block lands,
before its record is written, so that a block put back after all is put
back as any other. A search in the block measures the waiting ones
exactly, as it does a server's backlog, and a rollback drops those its
undo made tombstones (`forget_waiting`) -- the newest of the waiting, since
nothing else leaves one while a block is open. A lone statement links as
it did. Linked a row at a time, as a `/batch` of single puts would have
them, 100 000 128-dim rows went in at 5.3k rows/s with the graph kept;
linked together, at 16.0k, and 18.3k as a `/batch` of single puts over HTTP now (`make load-bench`).

**A server answers before its graph is linked.** A server checkpoints only
on its way down, so a crash after a long run leaves every vector written
since in the tail, and linking them at the open kept the port closed for as
long as they took: at 100 000 x 768 never checkpointed, `fenec-server` answered
its first `near` 67.7 s after it started, and linking at the open still
takes 18.4 s with the lists pruned in parallel. `fenec-server`, a tenant and a replica
open with `fs::open_serving` instead, and it answers after 1.23 s: those
vectors go into the arena unlinked (`VectorIndex::defer_batch`, every vector
of a graph the open cannot restore too), a search measures each of them
beside what its walk finds -- so an answer is never missing one -- and
`fenec_http::link::beside` links them on a thread of its own, slices of
about 10 ms under the write lock, each at most twice the last (a pace taken
over a small graph had a slice hold the lock for 112 ms). `near` takes the
exact scan's 10.5 ms until the 19.2 s of linking are done and 0.31 ms after,
recall 0.976 against 0.978 (`make reopen-bench`). A checkpoint meanwhile
writes the waiting nodes flagged -- graph record version 5 and only then in
the varint layout, a node's flag in the flat one -- and an open that does not
defer links them there. The linking needs the
lock to let a waiting writer in: Linux's std lock does, while on macOS
readers slip past it, and four clients asking back to back kept it from
finishing in eleven minutes -- as they would keep any write waiting. The
browser has none of it (`vector::UNLINKED`: 1.1 KB brotli).

**A server keeps its graphs in its file.** It checkpoints only on its way
down, so a crash after a long run left every vector written since the start
to link again. `fenec_http::link::keep` looks at every database the process
serves every 5 s, and appends to the tail each graph that changed in 10 000
nodes since it last reached the file, has none waiting to be linked, and
whose record the file has grown three times over since
(`Database::save_graphs`, under the read lock, which keeps the writes out
while the graph is written). A database whose lock is taken is passed over
until the next look (`try_read`): a block of 100 000 128-dim vectors loaded
with the graph kept held the keeper 3.0 to 4.3 s, and every other database
it keeps with it. The record holds the whole graph, so a bound
on the linking alone would have a big graph written over and over for a
little of it; waiting for the linking kept a crash from leaving the nodes
waiting again. It is a graph record (kind 4) of version 6, 8 laid out flat,
which an older binary does not know and rebuilds from -- it restored a record in the tail
against the documents the whole file left, and a vector rewritten after it
kept the links of the one before -- and like the history it moves no
counter and no replica is sent it (`Tee::append`). At 100 000 x 768 a crash
leaves at most 10 000 vectors to link, 2.6 s with `near` at 1.56 ms p50
meanwhile, where every vector waited 19.1 s in the same run with `near` at
the exact scan's 11.96 ms; the ten records were 33.8 MB of a 372.7 MB file
until the next checkpoint. Every write waits out a record's read lock, so
its lists go out a part of `LIST_PART` nodes at a time on every core
(`put_all_lists`), each link as its eight bytes cut to its width rather
than a `memcpy` of the width, and the file's sink leaves the record to the
durability run once the lock is let go (`Sink::append_deferred`) rather
than write its megabytes under it: a record held the lock 5.0 ms p50 and
7.9 at most, against 18.3 and 33.3, the same bytes (`make reopen-bench`,
in turns).

**`/_changes` reads the writes on disk, documents and all** (`cdc.rs`,
`fenec-server --cdc`; a primary's feed with `--replication-token`). A
subscription keeps a query's rows and is reseeded past its ring of ids,
which holds no documents; change data capture has to see every write once.
So it reads the records the feed keeps for replicas (`Feed::changes_after`:
from the record holding the write after the cursor, where a replica is sent
an image) and has the engine read them out a write at a time
(`Database::changes_in`, native only): numbered as the counter numbered
them, each document by its collection's schema as the database knows it or
as a create among the records made it. `since` is the last write a
consumer has and `Fenec-Next` the last one an answer holds; a cursor inside
a block's record goes on after the write it names. `Fenec-Seq` on every
answer is the last write the database holds (`Feed::seq`, read after the
page, so never short of it), on disk or not and of the collections the
stream leaves out: a `Fenec-Next` that has reached it has every write
there is, where an empty page alone could not tell a consumer that had
caught up from one whose writes were not on disk yet, and Kestrel's
worker waited out 600 ms of empty pages to be sure. Only what an fsync
covered is handed over, a cursor the feed no longer reaches is answered 410
with the first `since` it does -- never with writes missing -- one past the
last write 409, and `wait` waits on the feed's `Condvar` for a write. A
scoped token is refused: its filter could not hold back the deletion of a
row it never saw. A consumer with no state of its own has the server keep
where it is (`/_changes/consumers/<name>`, rows of `_consumers`, made by a
`POST` at the last write on disk -- read from "now" each time, it missed
what came between two reads -- and moved by a `POST` of `since`), each
write at least once; the stream leaves `_consumers`' own writes out, the
cursor going past them, and a `wait` waits on past them rather than answer
nothing at once, while a commit to where nothing but cursors were written
since the consumer's place writes nothing (`unmoved`, the change ring's
`changed_collections_since`): a sink that committed every answer
committed its own commit's empty answer, 30 000 fsynced writes in a few
idle minutes. A feed kept for it alone has no token (`Replication`'s
token is an `Option`: an empty one matched an empty `Bearer`). Keeping it
cost nothing measurable, 16 400 single puts a second over HTTP either way,
and 9 000 rows of 128 dimensions read back at 283 000 a second.

**An `Idempotency-Key` makes a write once** (`idempotent.rs`). A `put`
with no id makes a row each time, so a retry after a timeout wrote it
twice. A keyed REST write, `/query` or `/batch` runs as a block under the
write lock, and its answer is kept in the same block as a row of
`_idempotency` -- the key the subject's for a scoped token, and the
request's hash beside it -- so the write and its key land together, a
second request with the key waits for the lock and is handed the answer
(`Idempotent-Replayed`), the key with another request is 422, a failed
write keeps none, and a `compact`, which cannot be put back, takes none.
Kept for `--idempotency-ttl` (a day): `at` is `@ttl`, so a key past its
time is out of every read and the sweeper deletes it; the change stream
leaves them out.
A write without a key is rendered after the lock as before; with one, 16
200 single puts a second over HTTP against 18 300 -- with its statements
parsed each time and the collection made if missing every time, 14 600.

**A read can wait for a write on a replica** (`after` in `lib.rs`,
`Hub::reached`). A write's answer carries `Fenec-Seq`, the change it left
the database at -- a field of `Response`, written into the head with no
allocation -- and a request sent with `Fenec-After: <n>` is served once
the database holds change `n`, or answered 504 with where it stands after
`Fenec-Wait` (5 s, 30 at most): never from before the write. A replica's
applied records reach the hub through `Database::apply`'s watcher, so the
wait is the subscriptions' `Condvar`. Held back, a replica answered a read
without the header from before the write and with it after 300 ms, as the
test holds it. It costs the server nothing measurable -- 37 500 puts a
second against 37 700 from a client that does not parse headers -- while
Python's `http.client`, parsing one more, went 18 200 -> 17 900.

**`insert` never writes over; `put` does.** `Statement::Put` carries
`insert` (the parser sets it for the keyword, `POST /<name>` over REST):
a document naming an id the store holds is `Error::Duplicate` -- 409 over
HTTP, as `Exists` is for a collection -- and the statement, a block of
one, is put back whole. JWT scoping keeps
the flag as it rewrites the statement; made a `put` there, a scoped insert
would write over. The JS builder's `.insert()` still sends `put`: the sync
layer writes rows back through it when it undoes an optimistic write.

**A `set` reads the row it writes, and every write says what it did.**
`Expr::Arith` is `+ - * /` (`query::arith`): ints stay ints and one past
64 bits is refused, never wrapped; `/` between ints divides whole toward
zero; an int and a float make a float, refused once not finite; a
timestamp moves by milliseconds; a null is null (`coalesce(n, 0) + 1`
counts from nothing); `+` joins two texts and only two (`$1 + ":dr"`, an
entry id made of its movement's, which travelled as a parameter of its
own) -- `+` rather than a `concat()`, since every client language joins
text so and the builders' `expr` already passes it, and a text and a
number stay a type error, never a number made text unasked; anything
else is a type error, and the result meets
the field's type check as a literal does. The lexer reads `-` after what
ends a value (`n-1`, `n - 1`) as a subtraction and before a value as a
number's sign (`subtracts`), and the parser folds a `-` over a number back
into it, so every text that parsed before parses to the same tree;
`arith_level` is two frames a level of parentheses, `operand` out of line
so `primary` is in one place (inlined three times it was 2.9 KB of the
browser module). A `set`'s pairs are bound once a statement
(`Assign`, `Calc`, as `Filter` binds a filter): a field by its position, a
value reading no field -- a literal, a parameter, `now()` -- worked out
once, the rest `eval` over the row, so an error comes at the first row and
never over none. The row is read once (it was read twice, for the values
and for the indexes it took out), and the registry is taken out of the
database for the statement so a row's expression can call it while the
row is written. Under the single writer the read and the write are one, so
an increment is atomic: 16 threads x 10 000 on one key end at 160 000,
320 000 a second, 16 HTTP clients the same at 102 000
(`make counters-bench`); over 100 000 rows `{n: 7}` took 85.5 ms
and takes 65.0, `{n: n + 1}` 66.6. `now()` is the database's
clock where one is set (`EvalCtx::clock`, the browser module's every
statement) and the system's otherwise, worked out once a statement. The
record holds the document written, never the expression: replicas,
`/_changes`, archives and a sync replica's server see the result, and a
replica's optimistic apply works the same text out over its own row while
the server works it out again -- two replicas' offline increments both land
(the scenario file holds it). `put ... if absent` (`Statement::Put`'s
`if_absent`, which sets `insert`) passes over a document whose id or
`@unique` value a live row holds and counts what it wrote -- `SET NX`
without a 409 that aborts a `/batch` or an exception to catch. A row past
its `@ttl` is out of a write's way as of every read: an insert naming its
id writes over it, and a `@unique` value it holds is let go of with it, the
row deleted in the same statement (`expired`, `erase`); left to the
sweeper, a lock that expired stayed held up to a minute. The builders
render `inc(n)` as `f: coalesce(f, 0) + $k` and `expr(text, ...)` with its
`?`s bound, every SDK to the golden file. The browser module grew
7.9 KB, 2.7 KB brotli: the parser's arithmetic, the binding, `if
absent` and the expired row's place. `site/content/docs/redis.html` is the
recipes, each FenecQL block run by `tests/redis_docs.rs`.

**`require <n>` makes a write's count a condition.** A write that matched
no row answered `affected 0` and its `/batch` went on: a transfer's debit
that found too little money was passed over and the credit made money,
2 400 transfers taking the sum from 200 000 to 411 409. `put`, `insert`,
`set` and `del` carry `require: Option<u64>` (the parser's `require` after
the statement, an integer and never a parameter, so a statement keeps its
shape), and `execute_inner` hands the answer to `required`, which refuses
any other count as `Error::Unmet` -- after the writes, which the block they
are in puts back as it puts back any statement that failed: a lone one's
block of one, a `/batch` whole, the browser module's `run` of several. A
clause of the write rather than an `assert (get ...)` statement: the count
is the write's own, taken under the lock that wrote it, with no second
read to race or to scope, and a scoped token's count is of the rows its
filter let it write. 412 over HTTP (`api::status_of`), apart from 409 so a
client tells a lost race from a value taken, and a `/batch` that stops says
which statement did (`"at"`, from 0, `render_batch_stop`), and so does
a text of several through `fenec-abi` (`Refused::Error`'s third field,
`"at"` in the module's and the native library's error, `FenecError.at`
from `Fenec.run`): a ledger's debit and credit are both `set accounts`,
and a page ran a text's prefixes again to tell which had stopped it --
202 bytes of the browser module, 8 brotli; `FENEC_UNMET` 14
over the native library, after the boundary's own 11-13; `unmet` in each
SDK's errors, and `{ require: n }` in every builder, held to the golden
file. A replica's sync sends it with the write and holds it locally too,
and refuses one that would reach a row of an unanswered insert
(`REQUIRE_UNANSWERED`): that row is reached on the server by a second line,
its key, which would split the count. `tests/require.rs` (both crates)
moves money between few accounts from eight threads in process and eight
HTTP clients as `/batch`es, a credit in eight to an account that is not
there: the sum stays and no balance goes below zero. The browser module
grew 1 479 bytes, 310 brotli. A `get` takes it too (`Select::require`,
parsed among its clauses, before a `lookup`): the rows it answers, after
`offset` and `limit` -- `limit 1 require 1` is "one exists" -- must number
`n`, or `read_required` refuses it as `Unmet` where the `get` is answered
(`query`, `execute_inner`; `query_json` leaves a required one to `query`),
and a `/batch`'s write lock makes the read a checkout's guard on the
writes around it. Not beside `count` or an aggregate without `group`,
which answer one row, nor in an inner `get`. Builders have it as a step,
`.require(n)`; a replica's sync refuses one beside a synced write
(`GUARDED`, the browser's `batch()` in the same words): the replica's
count, which the server landing the batch would never see. The module
grew 1 017 bytes, 471 brotli.

**`put ... if absent else set {..}` is an upsert; `put <c> $n` takes its
documents from a parameter.** A rollup's row is "made at zero if missing,
then added to", and was two statements a key -- `if absent`, then `set {n:
n + $k}` -- or a read of which keys exist before the block. `else set`
(`Statement::Put`'s `else_set`, only after `if absent`) sets the row
holding a document's id or first `@unique` value as `set` sets a row
(`Database::set_row`, which `update_rows` now calls a row), its values
over that row and `new.f` the document's `f` (`Calc::New`, `Upserting` for
an expression; `new.id` the id it names, or null; a name the collection
does not have refused, never read as null), and counts it with the rows
made; a row past its `@ttl` is made again, not set. The document's own
hooks run as an insert's, the row's as a `set`'s. A key twice in one
statement adds to the row the first made: the vectors written before a
row is set are linked first (`index_written`), or the batch at the end
put the first one's vector back over the set's. A scoped token needs
update beside insert for it, and the row it sets must pass its update
filter (`Check::before_overwrite` asks `admits` of the row written over,
as PostgreSQL's `ON CONFLICT DO UPDATE` asks the row its `USING`): found
by a value any row may hold, it was anyone's. `put <c> $n` (`docs_param`)
writes the parameter's object, or each of its list of objects, a member a
field (`documents_in`, `Database::document_of`): an object is read exact
from the start, as a body's json field is, and a list of numbers in it
becomes a vector's `f32`s through the field's type, so a beacon is one
text whatever its size -- a page's statements were texts of up to 500
documents, past the 1 KB the parse cache keeps. Scoped, `scoped()` writes
the documents in first (`Statement::with_documents`), so each is held to
the rules a written one is. A page of 1 000 events parsed and written
takes 2.30 ms as `put ev $1` against 5.31 written out; a rollup page of
1 000 keys, half new, 1.13 ms as one upsert against 1.47 for the read and
the two writes, in process (`make analytics-bench`'s `writes`). The
browser module grew 5.0 KB, 1.3 KB brotli -- 7.5 KB more while `new.` was
cut off a name by a slice that can panic (`strip_prefix` now). Every
builder writes it -- JS `upsert(docs, patch, { require })` and
`toUpsert`, Python's `upsert`/`to_upsert`, Go's `Upsert`/`ToUpsert` (the
documents a slice, so the patch comes after them), .NET's, Swift's,
Kotlin's and Dart's -- the documents bound first and the patch as an
update's (`inc`, `expr`), held to the golden file. A sync replica sends
the text as written and the server works the set out again, as it does an
expression; the replica, whose `@unique` is a plain hash, finds each
document's row by its id or the shape's key, runs the upsert by id (the
rest made under temporary ids, a key twice in the page the row the first
made) and reads the rows it sets first to put back (`Sync::upsert`,
`#applyUpsert`); a document naming neither is refused (`UPSERT_KEY`), in
both, which the scenario file holds.

**A `set` or a `del` picks its rows with `order` and `limit`, and
`returning` answers them.** A job queue's claim was a read of the ready
page and a compare-and-set of it, and two workers that read the same page
raced, one of them told only by an `affected` short of its page. `Update`
and `Delete` carry `pick: Option<Box<Pick>>` (boxed: no other write has
one), and `picked_ids` hands the filter, `order` and `limit` to a `get`'s
`page_ids`, so `set jobs {owner: $1, run_at: now() + 30000, attempts:
attempts + 1} where run_at <= now() order run_at limit 10 returning *`
walks `@sorted` to its page and writes it under the one lock --
PostgreSQL's `UPDATE ... WHERE id IN (SELECT ... FOR UPDATE SKIP LOCKED)
RETURNING *`, the single writer standing in for the row locks.
`returning` (`*`, or fields and paths) reads each row out of the store, a
`set`'s after it wrote them and a `del`'s before (`returned`), and
`required` counts the rows answered: `limit 1 require 1` is one job or a
412. A `take` statement of its own was weighed and not made: the same
clauses are a pop (`del ... order ... limit 10 returning *`), an `UPDATE
... RETURNING` anyone reads, and any picked write, where a `take` would be
a queue's alone. The lease is the ready time moved on, so one `@sorted`
field holds ready, leased and delayed jobs and a lapsed lease is ready
again with nothing to sweep. That needed `now()` in a filter to be a range:
`Expr::fold_now` works it out once in `answer_filter`, with the `+ - * /`
over it, and `Database::pin` judges the folded filter; left a call, the
walk tested every leased and delayed job after the ready ones and, short
of a page, gave up past an eighth of the collection for the scan. A
scoped token picks among the rows its update or delete rules reach, and
`returning` ANDs its read rules in (`Scope::returning`), as PostgreSQL
holds `RETURNING` to the `SELECT` policies; `WITH CHECK` and `update(..)`
grants judge each row as any `set`'s. A sync replica refuses one on a
synced collection (`PICKED`, the scenario file), since it would pick and
answer the replica's rows. Builders take `order` and `limit` before an
update or a delete, which a `limit` lets go unfiltered, and `returning`
as an option (JS and Python resolve to the rows, Go's `Result.Rows`,
.NET's `ExecResult.Rows`, Swift's, Kotlin's and Dart's `updateReturning`
and `deleteReturning`), held to the golden file. Over a million jobs a
claim of ten takes 18.7 us p50 against a scan's 132.7 ms; 16 threads
claimed and acked them all at 185.8k jobs/s, against 93.8k for read,
compare-and-set and read back, which lost 43% of the jobs it read to
another worker; 16 HTTP clients 138.5k against 48.2k, 77% lost (`make
queue-bench`). The browser module grew 5.2 KB, 1.9 KB brotli -- the
clauses' parse, the pick and its answer (`page_of`, `returned`, each out
of line so a write that picks nothing keeps its code), the fold, and
`page_ids` out of line now that two call it (+0.7 KB net); `returned`
reads by `select`'s places cost 1.4 KB more. A write or a read that picks
nothing measured as before in process, 200 000 statements a round: an
increment 1.15 us against 1.16-1.18, a lock renewed 2.15 against
2.13-2.17, a read by a `@unique` key 0.61 against 0.59-0.60, which taking
the time's check out did not move (0.61-0.62) -- where the code lands; a
`set` over 100 000 rows 1-2% slower in `make counters-bench`, whose runs
of one build move as much, and `make requests-bench` inside its spread.

**`@unique` is a `@hash` that asks its bucket before a write.**
`IndexKind::Hash { unique }`, written as index kind 8 so a binary from
before refuses the file rather than open it as a plain hash and take the
duplicates. `put`, `insert` and `set` ask `Collection::unique_clash` after
the hooks and before anything is written: the bucket the value would go in
holding another id is `Error::Duplicate` (409), the statement put
back whole -- and the id `put` handed out for the document handed out
again, since nothing of it reached the store for the block's mark. A block's
earlier writes are in the bucket as a write keeps a built index up, so it
may free a value and take it again; `null` is no value; equality is the
hash key's, so `-0.0` meets `0.0`. The index is a `Derived` like any hash,
and the first ask after an open builds it from the documents, which is what
makes the answer exact: an open costs nothing more (19 ms for an image of a
million rows either way), the first write 15 ms over 100 000 rows and 259
ms over a million, and a put after it 940 ns against 850 under `@hash`; a
collection with no unique field writes as before (848 against 852). `create
index ... @unique` over a value held twice is refused naming it and two of
its documents, under the lock or beside the database, where it is asked of
the index once the writes made meanwhile are in. `Database::apply` builds
it and asks nothing: the primary did. A scoped token is told the field
alone (`access::told`, in `within`): the clash names the other row's id
and echoes its value, which told alice that bob's profile existed, where,
and what it held.

**A `@hash` bucket holds its ids ascending, in runs** (`engine/bucket.rs`).
A bucket was a `Vec` in the order its ids came, and a row left it by
`retain`, a walk of the whole bucket: with a field of a few values -- a
country, a device, an event's name -- every delete walked a fifth of the
collection, and the `@ttl` sweep held the write lock 370 to 500 ms a
thousand rows of an analytics site's 594 000 events, every dashboard read
waiting behind it. Now an id is found by binary search, and past 512 ids a
bucket is runs of at most 512 (`Bucket::Runs`, behind a box so a bucket
is the size of a `Vec`), so a removal moves one run's tail -- the sweep
takes the oldest rows, which one ascending list holds first and would
move all of. A run left a quarter full joins the next where both fit; a
bucket of one run is a list again, so a `@unique` value's costs what it
did. Over 10 000 000 rows with a field of five values the sweep holds the
lock 0.79 ms a thousand rows against 825 (`make ttl-bench`'s `hash`); a
lone `del` among a million rows 1.09 us against 76 to 134, a lone `put`
699 to 708 ns against 708 to 754 (`writes`). Readers take a bucket's ids
in order through one iterator (`Bucket::iter`, `Ids`), so `lookup`'s
children no longer sort them, and the maps a group's or a facet value's
number is kept in are of the index's own type (`Bucket::one`): through
`flatten` and a second map type the change was 1.3 KB brotli of the
browser module. Its buckets stay one sorted list (the runs `cfg`'d out,
a removal a `memmove` of the rest): 390 bytes, 0.4 KB brotli.

**A hash index's table grows a 256th at a time, and a text index's
terms too** (`maps::Sharded`). A `HashMap` that is full moves every key
into a table twice its size in the insert that found it full -- under the
write lock, every request waiting -- and a `@unique` field holds a key a
row: `make recon-bench`'s journal passing 1 835 008 entries held one
transfer and everything behind it 120 to 137 ms in every run, and `make
growth-bench`'s 4 million puts one at a time took 7, 19, 49, 109 and
235-267 ms at each doubling from 229 376 keys. A `@text` index's terms are
the same map, and a field whose rows bring terms of their own -- an id, a
code, a name -- doubled it as often: 9-10, 23-25, 55-59, 125-132 and
276-324 ms over the same doublings (`growth-bench`'s `text`, four new terms
a row to 4 million). Past `SPLIT_AT` (114 688 keys, a table of 2^17
buckets' worth) the map is 256 maps, each growing on its own: the longest
put is the split's, 2.5-3.6 ms for a hash index's keys and 4.9-5.3 for a
text index's terms, against 6.2-8.2 and 8.5-10.6 while the split hashed
every key again. Each key is kept beside its hash (`Hashed`), which its
table takes as it is (`Pass`): a lookup is one SipHash under the map's own
random keys, and the shard is bits 49 to 56 of it -- below the seven a
table tags its buckets with: by the top byte, every key of a shard had one
tag and a probe read every key it passed. Picked by an `Fx` of the key, the
shard then hashing it again with keys of its own read from the shard, a
lookup in a loop over a million keys took 94-135 ns and over four million
121-144, against 68-87 and 83-93 now, and 33-42 -> 24-34 under the split;
an index built after an open is 15-25% faster, its tables growing with no
key hashed again. The cost is 8 bytes a slot: 48 -> 56 a hash index's, 80
-> 88 a text index's. A put's p50 is as it was (1.08-1.29 us), and so is
`match` (a new term 1.05-1.29 us against 1.09-1.37, three 1.37-1.65 against
1.41-1.72). Rejected:
one table taken into a new one 4 096 keys at a time, looked up in both
meanwhile -- lookups 163 ns, since a map that stops taking keys never
finishes its steps, and builds a third slower. The browser module keeps
the one map and its `Fx` (`cfg`), byte for byte the size it was.

**`in (get ...)` is answered before the query, as the list it is.**
`Expr::InSelect` holds an inner `Select`; `Database::answered` (from
`query` and `execute_inner`, and `explain` inside its plan) runs each
inner `get` once, with the statement's parameters, and puts an
`Expr::In` of its one column's values in its place -- so every place a
filter goes, and the planner's "every element resolves or the list goes
to the scan" rule, take it unchanged, and `eval` never meets one. Only
a statement holding one, or reading a collection whose rows expire, is
cloned: the rest costs a look at its filters. A null is left out of the
list; past `MAX_SUBQUERY_VALUES` (100 000) distinct values it is a query
error: an inner `get` with no `limit`, `group` or ranking lists its
matches' ids, as `count` does, and reads its column, the values told
apart by their encodings only once there are more than the bound
(`distinct_column`) -- cut at that many rows, 125 000 `buy` events of
937 users were refused, and told apart as they came, a bucket of 1 967
went 0.15 -> 0.36 ms; one with them is read one row past the bound; past
`MAX_SUBQUERY_DEPTH` (4) refused in the parser and the engine. The inner
`get` selects one column -- a field or one aggregate -- and no `lookup`
or `count` (`Select::check_subquery`). A scoped token's `scoped()` holds
each inner `get` to the read rules as any `get` (`Scope::inner`), and a
rule's filter takes none: `admits` tests a document on its own, where no
query runs. A long `in` list is looked up, not walked (`Test::InSet`,
`IN_SET_AT` = 8, over an int, a text, a timestamp or a boolean typed
field and `id`, the values' encodings in a `HashIndex` made at the first
row tested): 2 000 codes over a scan of 200 000 rows took 2 302 ms
compared a value at a time and take 16. One country's 2 000 customers'
orders by `@hash` take 0.83 ms against 0.72 written out; customers with
a rare order 0.20 against `lookup ... required`'s 0.44 (`make
subquery-bench`). A subscription's REST shape cannot hold one, and the
JS builder's `{ f: { in: query } }` renders one with the outer query's
parameters.

**`@ttl` is an ordered index whose rows expire, and every read leaves
them out at once.** `IndexKind::Sorted { ttl }`, written as index kind 9
and the milliseconds behind it, so a binary from before refuses the
file rather than hand out rows past their time; a timestamp field's
alone. Every filter over the collection -- each `lookup` level's, an
inner `get`'s, a `set`'s and a `del`'s, `changes_since`'s shape -- has
`not (field <= now - ttl)` ANDed in (`Database::alive`): a `not` so the
planner never takes it for a range, and a row whose field is null, with
no time to expire from, lives. `now` is the system's clock natively and
`Database::set_clock`'s where set -- the browser module's, handed
`Date.now()` (`db.now`) as `fenec_query`'s last argument, and refused
without one rather than answered at a time it was not. A server's
`fenec_http::sweep` looks at every database it serves once a minute and
deletes what is past its time, a range of the index found under the
read lock (`Database::expired`) and 1 000 rows deleted under the write
lock as a block of ordinary deletes (`Database::sweep`, the filter
written there, since a `del` leaves expired rows out): replicas,
`/_changes`, archives and subscriptions see deletes. A replica, a
following tenant and the browser never sweep. `expired()` in a filter
reads the rows past their time (`Expr::asks_expired`, answered in
`answer_filter` as `field <= now - ttl`, a range of the index, the alive
test left out; `not expired()` is the default read), in every place a
filter goes, `set` and `del` too: a reaper's, which gives back what a
lapsed hold reserved -- `del holds where expired() and id = $1 require 1`
beside the give-back, one block, the delete the once-only guard -- where
a ledger kept holds `@sorted` and reaped by hand, since a row the sweep
deleted unseen kept its money in `held` for good. The sweeper deletes a
row a sweep's period past its time (`sweep::GRACE`, `pass_after`), so a
reaper as frequent sees every one; at the first pass after its time a row
that lapsed a moment before went unseen whatever the reaper's period. A
scoped token reaches them only by the policy's `expired` grant
(`Scope::reaping`), never through `write` or `*`; a rule's own filter
takes none. A collection without `@ttl` refuses it by name (the registry
has no `expired`, and says so). 100 000 rows past their
time out of 200 000 went in 0.56 s, the lock held 1.06 ms a batch at the
median and 2.3 at most; a read tests each row's time, so a `count` over
a million rows half past their time is a scan, 68.8 ms against 1.1 with
no expiry, and a collection with none reads as it did (`make
ttl-bench`). `create index ... @ttl(d)` gives a timestamp field one, and
`alter collection c alter field f @ttl(d) | @sorted` sets or takes it
off (field change 4, the index kept). `_idempotency`'s `at` is the first
user: its purge on the write path went. The two features cost the browser
module 16.4 KB, 5.7 KB brotli -- 18.7 KB more while a ttl printed back
through a float, which brought the standard library's float formatting.
`expired()` cost it 561 bytes, 98 brotli.

**`alter collection` rewrites no document; positions are not places.** A
document is its values in field order, so a field added goes last and a
document written before it ends before its place -- `skip_field` skips
nothing at the end, and the read is `null` (`Schema::read_doc`,
`Store::read_field`, `read_fields`); a rename is the schema alone; a drop
leaves the place in `Schema::dropped`, written as a nameless field of type
tag 12, skipped on read, written as `null`, refused by name, its index
taken off (`Collection::take_indexes`), until `compact` writes each
document without it (`strip_dropped`, the kept values' bytes copied: 95
ms over a million rows in memory against 56 for a compact with none). A
field's position -- what `field_pos` gives and every reader passes -- is
its index in `fields`; its place in a payload is past the dropped ones
before it (`Schema::place`). The store keeps the dropped places
(`Store::set_dropped`, set wherever the schema changes) and works a
place out per read for `read_field`; `read_fields` takes places, worked
out once a query by its callers (`Filter::new`, the aggregate, the
rebuild): worked out a row, the compare took a million-row scan 21.2 ->
21.8 ms. Natively the end is told inside the skip's own tag lookup
(`skip::<true>`); asked in the loop, a scan reading two fields three apart
took 39.1 -> 40.4 ms; the browser module asks the compare, a second copy of
the skip being 1.1 KB of it. After both, five scans of a million rows went
from 1.7% slower to 1.7% faster (21.2 -> 21.6 ms, 36.8 -> 36.2, 39.1 ->
39.3), as the same build with `read_fields` written as before did (+1.3%
to +3.1%) -- where the code lands, not what it does -- and the browser's
1.70 -> 1.71 ms. An alter is record kind 12 -- `[change][field]
[new name][schema]` (`FieldChange`) -- not a kind-5 record with a change
byte, which a binary from before would read as an index added; it counts
one write, rides in a block, and is undone as an index built is
(`Undo::Altered`: the schema before, a dropped field's indexes to put
back). The load keeps a graph restored before it under its field's new
name; a replica applies it through `Collection::alter_fields`, as the
write path does, checking nothing; `changes_in` reads the documents after
it by the schema it made. A field added under a dropped one's name is a new
field: the old values stay in the dropped place. A type change is refused:
it would rewrite every document under the write lock. A compact over a
collection holding a dropped place holds the lock (`compact_online`): the
copies made beside the database are the records as they stand. An add, a rename and a drop take 0.025,
0.021 and 0.004 ms over a million rows; the browser module grew 12.2 KB,
4.0 KB brotli -- the reading side, which a file from a server needs, and
the writing side, which a page's own database does.

**A `json` field holds any value, and a path is a name.**
`DataType::Json` (type tag 14) holds an object (`Value::Object`, value tag
13 -- 11 and 12 being a collation's and a dropped place's, in a number space
the two share), a list or a scalar; a typed field still refuses an object.
An object's members are sorted and unique (`Value::object`: a key twice is
refused, never one of them kept), so a member is found by binary search, two
equal objects encode alike and a hash index files them as one. A field's
name holds no dot (`Schema::new`), so a path -- `meta.source.rank` -- is an
`Expr::Field` whose name `Schema::path_of` splits, and every place a name
went takes it unchanged: `select`'s columns, `order`'s keys, `Fields`, the
planner's equalities and ranges; an `Expr::Path` would have been a variant
for every match over an expression in six crates. A path that leads nowhere
is `null`. `Filter` binds one as a field (`Slot::Path`) and reads it off the
row's bytes (`Store::read_paths`, `codec::decode_path_into`: the members
before the key passed over undecoded, a text into the text its slot held);
the reading is out of line and `matches` inlined natively, since left to the
compiler a scan by a text field went 3.05 -> 3.17 ms with no path in the
filter at all. A path's index is a field of its own beside the fields
(`Schema::paths`), written inside its json field's schema entry, and kept up
from `Document::at`; `Collection::fit_paths` makes the structures match the
schema after an alter or an undo, unbuilt, rather than move them by name.
`@hash` files a whole float as the int it equals (`hash_key`), and a lookup
over json takes only what a bucket answers exactly (`lookup_key`: not an int
past 2^53, not a timestamp); `@sorted` keys numbers as `f64`s and text by its
bytes, numbers first, and holds apart what it cannot order -- a boolean, a
list, an object, an int past 2^53 -- answering nothing while it holds one
(`SortedIndex::answers`); `tests/json.rs` holds both to a twin collection
row for row. Nesting is 64 levels and a path 64 keys, refused past either
(`MAX_JSON_DEPTH`, `MAX_PATH_KEYS`), the JSON reader's recursion bounded
with them. A json field keeps a number as written, and a list of numbers
alone is read the quick way, into a vector's `f32`s, by every reader that
has no schema -- the lexer (`Tok::Vector`), a query's parameters, the
browser module's vectors handed apart -- so a vector given to one, or
compared with a path into one, is refused (`json_value`,
`Database::refuse_inexact`) and the caller reads that part again as
written: `Database::exactly` names it (the text, or parameters by place),
`fenec_ql::parse_for` / `parse_exact`, `json::parse_params_exact`,
`/query`, `/batch`, a REST `where=`, and the module, which answers
`"exact": [places]` for `run` to send those as JSON. A statement with no
list of numbers costs the walk of its literals, one into a collection with
no json field a look at its fields: 0.03 us for a `put` of 1 000 128-dim
rows, 14 us with a json field beside the vector, against its 9.0 ms, which
parsed and ran as before natively and in the browser module (`make
wasm-speed`'s `put`). A typed array handed to the module goes as the
numbers it holds, each an `f32` exactly. An object, an HTTP body
(`parse_documents_json`) and an imported `jsonb` cell (`json::parse_json`)
are read exact from the start. Over 100 000 documents a path
filter scans in 5.7 ms against a text field's 4.2, 0.025 ms through `@hash`;
a scan with no json field moved by noise alone. The browser module grew
29.8 KB, 9.6 KB brotli -- 3.7 KB of it reading a list as written, a second
lexer 2.9 KB of that until it took its flag at run time there
(`lex_with`); the standard library's sort for the members (4.1 KB) and a
walk generic over its callback (3.8 KB) were taken out on the way.

**A vector written again is one node** (`VectorIndex::place`,
`aliases`, `Same`). Written hundreds of times, a node each filled its
neighbours' lists with copies -- the diversity rule takes a candidate at
distance 0 from the owner, `0 < 0` being false -- and a walk could not
leave them: 1 812 of 6 019 copies over 20 000 x 128 found by no search,
other queries' recall 0.82 -> 0.67. Skipping copies in the heuristic took
the recall back to 0.72 and orphaned the copies, since no list took them
in. Now every insert, batch and deferral goes through `place`, which
finds a live node storing the same vector to the bit -- `Same`, open
addressing over `node + 1` beside the hash's top half, made on the first
insert rather than at an open, 16 to 32 bytes a node -- and makes the
document one of its `aliases` (`(node, doc)` sorted, not a map: another
hashbrown in the browser) rather than a node. A search hands a node out as
its documents in id order, the exact paths by `(node, doc)` pairs put in
node order through the `u64` sort; a document leaving a node hands it to
another holding its vector, and only the last makes it a tombstone
(`detach`). Over codes the stored codes are compared, which `near` puts in
order by the documents' own vectors anyway. The level is drawn only for a
new node, so a graph with no vector twice is byte for byte the graph it
was. Graph records 9 and 10 carry the aliases after the tombstones (and
natively the copies table's hash halves after those, below); a restore
checks each alias's document holds the node's vector. Tests whose
data repeated vectors (`i % 13`) to count nodes and tombstones now write a
vector a row. The browser module 2.2 KB brotli; builds, searches and
opens as fast.

**Natively the table that finds a copy grows from its own slots**
(`Same`, `SameTable`). In the browser `Same` is one table, made again from
the arena once it is half full; natively that was the write that found it
full hashing every live vector again under the write lock -- 118-132 and
242-281 ms at 2^19 and 2^20 nodes of 128 dimensions, 166-169 and 376-377
ms at 2^17 and 2^18 of 768 and 1.45 s at 2^19 (`make growth-bench`'s
`vector`). A slot's place is the low bits of the half of the hash it
holds, so a table made twice the size reads its slots and no vector,
leaving the tombstones' behind, and past `SAME_SPLIT` (2^16 slots, an
index of 32 768 nodes) the table is 256, by that half's top byte, each
growing on its own: the split moves its slots in 0.72-0.80 ms and a shard
at 2 million nodes grows in 0.15, and over 1.1 million 128-dim puts one at
a time and 300 000 768-dim ones no put at a doubling took 10 ms. A place
taken from the half leaves fewer of its bits to tell two vectors apart --
a slot of another vector at the same place passes the half's test once in
2^(24 - k) for a shard of 2^k slots, a vector compared for nothing about
once in 4 000 puts at a million nodes. Where its graph record carries no
hash halves (below), the first put after an open makes the table from the
arena, every vector hashed -- made at the open it would cost a database
that only reads, and under `--warm` a read-only server 16 to 32 bytes a
node -- on every core, `SAME_SHARE` nodes at a
time, through four chains of multiplies and MurmurHash3's finish
(`hash_lanes`) where one chain waited on its multiply a word (a 768-dim
vector in cache 1.18 us against 0.17): 275-441 ->
33-169 ms at 1.1 million x 128, 444-741 -> 24-175 ms at 300 000 x 768, and
1.18-1.37 -> 0.31-0.59 s at 600 000 x 768, where on this 8 GB machine the
arena and the file outgrew memory. An open whose tail holds vectors makes
it there: `make reopen-bench`'s deferred open of 100 000 x 768 never
linked 0.59-0.75 -> 0.35 s, the linked one 16.2-17.3 -> 14.9-16.3 s. A
put's p50, a batch's build (100 000 x 128 in 3.63-3.74 s against
3.72-3.85), `near` and an open of a checkpointed file are as they were.
The browser module keeps its table and its hash (`cfg`): 618 010 bytes as
on main, every function the size it was.

**A graph record carries the halves the copies table is made from**
(`HALVES`, `Same::halves`). The first put after an open read and hashed
every vector to make the table: 33-169 ms over 1.1 million of 128
dimensions. Hashed as the restore fills the arena, each vector in cache,
the table would cost no put, but the open of a checkpointed 100 000 x 128
took 0.45 ms longer (3.8%) and of 100 000 x 768 2 ms (6.7%), and four
vectors' chains side by side hashed only 1.65 times as fast. So natively a
version 9 or 10 record ends with a `HALVES` byte and the top half of each
node's hash, 4 bytes a node, a tombstone's 0: 0.68% of a 128-dim file,
0.13% of a 768-dim one. The restore keeps them -- once 64 live nodes
spread over the arena hash to theirs (`keep_halves`) -- and the first
vector placed makes the table from them with no vector read, gathered by
shard and each shard's table filled on whichever core is free
(`Same::spread`: 1.1 million halves in 4.0-4.4 ms, where put in where
they fell they took 10.5-10.9). The first put after an open went 4.7-5.5
-> 3.4-4.2 ms at 100 000 x 128, 12.8-15.8 -> 6.4-9.7 at 100 000 x 768,
30-244 -> 7.6-8.5 at 300 000 x 768, and in `make growth-bench` 47-160
-> 16-31 at 1.1 million x 128 and 68-312 -> 4.4-27 at 300 000 x 768;
the opens as they were, 11.65-11.98 against 11.68-11.83 ms and 28.2-29.1
against 28.7-30.1 in turns, and the puts after (growth-bench's p50 264
and 263 us, 690 and 676). What is left is the first growth of the arrays
a restore sized exactly and the caches an open leaves cold. On this 8 GB
machine an open that paged the arena out had main's first put, reading
every vector, bring it all back; now the puts after it fault it in as
they walk -- 3.4 -> 8.4-9.7 ms p50 at 300 000 x 768 there, the first
twenty puts 339 -> 224 ms in all. A
database that is only read keeps the halves, 4 bytes a node -- 0.6% of a
128-dim index. A writer has them from the restore, or reads them off the
table's slots on every core past `SAME_SPLIT` nodes (`halves_by_node`:
8.3 -> 2.3 ms at 1.1 million, under a server's read lock as it keeps its
graphs), or hashes the arena where neither holds them, so a record is the
same whatever way its index came to be. The record keeps its version: the
reader before stops at the aliases, so the browser, whose hash is
another, and a binary from before read it without them -- this build
refuses anything else there -- and a record without them, the browser's
or a binary's from before, has the table made from the arena. The browser
module is the same code to the byte, and its first put after a load still
hashes the arena: 12.8-15.0 ms at 20 000 x 384, the puts after it 0.31.

**An archive and a backup can be sealed** (`seal.rs`, `fenec key`,
`--key-file`). A copy in a bucket is out of reach of the disk's
encryption, so with a key every archive file and a backup file is
ChaCha20-Poly1305 (`crypto.rs`, RFC 8439's vectors and 2 000 random cases
against node:crypto): ChaCha20 over AES-GCM because it has no secret-indexed
tables in software, Poly1305 in 26-bit limbs. Frames carry a random nonce
and their place and last-ness in the AAD; an image and the history are
sealed whole (`open_whole` refuses a file cut short), a segment a frame for
each batch of writes, whole records in each, so a crash cuts off a frame as
it cut a record (`open_appended`, the segment's `sealed` state). A file of
the other kind than the archive is refused. About 400 MB/s either way on
one core. The live file stays plain -- mapped and read in place -- for the
disk to encrypt. A checksum a record was measured and not taken: hardware
CRC32C was 45 ns of an 840 ns put at 530 bytes, 357 at 3 KB, under the write
lock, and the browser has no hardware CRC.

**Refusals and schema changes are logged; a refusal waits**
(`fenec_http::audit`, `--audit`, `--auth-delay`). A JSON line an event: an
HTTP 401, a statement whose shape starts with
`create`/`drop`/`alter`/`compact` -- found where `statements::record` shapes
every statement, so no literal reaches the log -- and a
`/_admin/` or `/_shard/` request that changes something. Who is a
thread-local set as the connection starts, a connection being a thread. A
refusal waits 100 ms, doubled for each more from its address within a
minute, 5 s at most; a good token clears the count, asking the table only
when an address has one (`FAILING`), so the hooks cost a request 12 ns.
Behind `fenec-shard` every request came from the router's address -- four
forged tokens made a fifth client's expired one wait 1.6 s, and any good
token cleared the attacker's count -- so the router waits out a refusal
itself, by its client's address (`forward` calls `audit::http`), and a node
waits none for a request bearing `Fenec-Router: <address> <mark>`, the mark
16 bytes of an HMAC of its admin token (`audit::router_mark`,
`Config::router_mark`), the address the log's `peer` (the router `via`):
the router drops the header from what a client sends, and a node believes
it from no one else. At the router the count is one across every node.
As two headers and the whole HMAC it added 2.5 us to the 22 a request
through the router took; as one, with the forwarded headers borrowed
rather than cloned and written without `format!` (`Pool::send`), 0.85 us
on average over four turns against main, inside its spread of 21.2-23.0. A browser's preflight is
answered 204 before any token is asked, where `--http-cors` lets its origin
in (`cors_allows`): authenticated first, it was 401 and a page on another
origin could send nothing.
Reads and writes are no events. A test that opens the log is a `[[test]]`
of its own: the log and the counts are the process's.

**Every request has an id, in its answer and in every line it wrote**
(`fenec_http::request_id`). Taken from `X-Request-Id` when it is printable
ASCII of at most 128 bytes, made otherwise -- 16 hex characters,
SplitMix64's finalizer over a key read from the system once a process, the
thread's number and its count of requests, so no system call a request --
and set as a thread-local where the request is read, as the audit log's
peer is: `Response::write`, a subscription's and a replication stream's
head send it, `audit::line` writes it as `request_id`, and `log!` ends a
line with `request_id=<id>` while one is set. The slow statements are JSON
lines on stderr through `audit::line` (`event` `slow`, `duration_ms`,
`kind`, `tenant`, `statement`), so one parser reads both logs, and every
line has a `level`; an admin event's status is `http_status`, since
Datadog takes a `status` for the level. `--slow-ms 0` logs every statement,
as PostgreSQL's `log_min_duration_statement = 0`. The router takes the
client's id or makes one, drops the client's header, and `Pool::send` sends
the thread's id with every request to a node, which takes it as its own;
the router's answer carries its own id in place of the node's. An id costs 15 ns
to make and 12 to keep a client's; in `make requests-bench`, eight
runs in ABBA turns a minute apart, one client's p50 stayed 0.016-0.022 ms
for a row by id either way, and eight clients' medians moved -12% to +6%
by case with no sign shared -- the runs' own spread, the machine busy with
other builds. `fenec_refused_total` (and the router's
`fenec_router_refused_total`) count the 401s the audit log names.
`monitoring/datadog/` holds the Agent's `conf.yaml` (OpenMetrics check,
`namespace: fenecdb`, `raw_metric_prefix: fenec_`), `pipeline.json` and
`dashboard.json`: `fenec-shard`'s `tests/datadog.rs` scrapes a primary, its
replica, a tenant node and a router in process and fails on a metric the
check keeps or the dashboard asks for that none of them sends, and
`fenec-server`'s `tests/logs.rs` on an attribute the pipeline or the
dashboard reads that no real line has.

**A request is a trace, sent over OTLP** (`fenec_http::trace`,
`--otlp-endpoint`, `--otlp-header`, `--trace-sample`, the standard `OTEL_*`
variables). A W3C `traceparent` is continued -- a sampled parent followed
whatever the ratio (`parentbased`), `tracestate` passed on unchanged -- and
a trace with none starts at the ratio, its id's low 56 bits against it
(`TraceIdRatioBased`, so a router and its nodes keep the same traces). The
trace is a thread-local, as the request id is: `begin` where a request is
read past the health check, the scrape, the preflight and the studio's
files (most of the requests and none of the time), `end` once the answer is
written, `discard` before a subscription or a replication stream takes the
connection over (its spans would be held for as long as it lasts), a trace
64 spans at most. The spans are the waits: the server span (`http.route` a
template, `db.query.text` the statement's shape from `statements::record`,
never a value -- a REST target's query string holds words the shape keeps,
so it has its route alone -- `fenec.request_id`, `fenec.tenant`),
`lock.wait` in `held::read`/`write`, `execute`, `durability` in
`await_durable` holding the `Tee`'s `fsync` and `replication.wait_sent`,
`fenec.wait_for_write` for `Fenec-After`, and the router's `forward`, a
client span whose id `Pool::send` sends the node as its `traceparent` --
the client's dropped from the forwarded headers while the router traces,
passed through as they came while it does not. A finished trace goes into
a queue (`OTEL_BSP_MAX_QUEUE_SIZE`, 2 048 spans) under a mutex held for a
push, the exporter taking it whole in one swap; full, the spans are dropped
and counted (`fenec_trace_spans_dropped_total{reason}`), so a request never
waits on the collector, and a thread posts it every second or at 512 spans
as OTLP/HTTP JSON written with `json::escape_into` and a client of a page,
the connection kept alive, a post bounded by `OTEL_EXPORTER_OTLP_TIMEOUT`.
fenecdb speaks no TLS, so an `https://` endpoint and a `grpc` protocol are
refused at the start: the endpoint is a collector on loopback -- the
OpenTelemetry Collector, the Datadog Agent's OTLP receiver -- forwarding
over TLS. Off, each hook is one relaxed load: in `make requests-bench`,
six runs in turns against main, one client's p50 stayed 0.022, 0.034, 0.059
and 0.022 ms (main 0.022, 0.033, 0.059, 0.021) and eight clients' medians
inside the runs' spread; on at 100% to a sink on loopback 0.022, 0.034,
0.060 and 0.024, eight clients 2 to 9% fewer requests a second, the
exporter's JSON a core of the eight. `fenec-server` grew 49.7 KB,
`fenec-shard` 82.7 KB (aarch64-apple-darwin), and the browser module not by
a byte: nothing below `fenec-http` changed. `fenec-server`'s
`tests/tracing.rs` holds the spans an in-process receiver (`tests/otlp.rs`)
is sent to OTLP's JSON, their parents and attributes, a ratio of 0 to
nothing but a sampled parent's trace, and a collector that hangs or is down
to costing no request its time; `fenec-shard`'s, a `[[test]]` of its own
since tracing is the process's, a trace through the router into the node.

**Limits error, they do not truncate.** `near` results cap at 10 000 rows
(`limit + offset`), expression depth at 512 levels, a `lookup` chain at 8 and
an `in (get ...)` at 100 000 values and 4 levels; all of them return a query
error, because a silently cut result is a wrong answer
believed right. Full table in `site/content/docs/limits.html`.

**Threads are `cfg`'d out of WASM.** The parallel HNSW build path must not enter
the wasm32 target. Likewise `now()` errors there — wasm32-unknown-unknown has no
clock, so time is passed in as a parameter.

**The wasm32 build has SIMD, and its kernels match the scalar ones bit for
bit.** `.cargo/config.toml` turns on `simd128` for that target, and
`vector::simd` holds the distance kernels written against it: the module is
built at `opt-level = "z"`, where LLVM does not vectorise the scalar strips
(a 20 000 x 384 HNSW build went 28.0 -> 9.95 s). They keep the scalar loop's
eight accumulators and reduction order, so a graph built in the browser is the
graph built natively; `web/fenec.test.js` checks that order against a
`Math.fround` reference. The same config links the module with
`--compress-relocations`: wasm-ld wrote each call's function index and each
address the code takes as a padded five-byte LEB, 36 KB of the module and
2.2 KB brotli, and V8, which tiers a function up by its size in bytes, runs
the smaller one faster -- `near` 12%, a page of vectors as JSON 15% (`make
wasm-speed`). A build that keeps the names strips the debug information
the flag refuses (`strip = "debuginfo"`, as `make size-report` does).

**aarch64's strips are written out as well, to the same bits.**
`vector::neon` holds every distance strip on aarch64 -- f32, f16 and int8
codes -- over one `strips`, eight accumulators as two four-lane registers,
each lane doing the scalar loop's multiply and add (no fused multiply-add)
and the lanes summed in `strip8!`'s order. Left to the vectoriser, the f32
and f16 strips read through four-way de-interleaving loads into two-lane
registers, half of NEON's width: a 128-dim dot product took 21 ns in cache
against 9.4, and 91 against 60 out of an arena of 100 000; the int8 ones
chained every strip to the one before. f16 is widened with `half`'s own
masks, shifts and multiply, not NEON's conversion, which would turn
infinities and NaNs into what `half` does not.
`f32_and_f16_kernels_match_the_scalar_strips` holds them to the scalar
strips bit for bit. A 100 000 x 128 build went 5.2 -> 4.3 s (f16 6.0 ->
4.5), and `near` at 768 dimensions over f16 0.54 -> 0.34 ms p50. x86_64's
vectoriser keeps four-lane SSE registers and is left as it is.
A walk measures a node's unvisited neighbours four at a time there
(`distances4`, `Arena::dists_to`) and takes them in the order it did: read
together, four vectors wait on memory once -- out of an arena of 100 000 a
128-dim distance took 60 ns alone and 47 four at a time, a 768-dim one 242
and 149 -- so `near` at 768 dimensions went 0.35 -> 0.29 ms p50 over f16
and 0.35 -> 0.28 over f32, a build 3-10% faster, and
`the_walk_scores_as_the_exact_search_does` holds the walk's scores to the
exact search's bit for bit. The browser's module built a graph 4% slower
four at once and grew 9 KB, and gathering the neighbours before measuring
them one at a time still cost its `near` 7%, so everywhere but aarch64 the
walk is as it was (`cfg`). There the walk also asks for the next
candidate's list and the fresh neighbours' vectors ahead of reading them
(`prefetch`, a `prfm` hint): each read was a cache miss waited on after
the one before, and a 100 000 x 128 build went 3.96-4.25 -> 3.58-3.80 s,
`near` 0.10 -> 0.08 ms p50. The diversity heuristic measures a candidate
against four chosen neighbours at once (`any_nearer`), 5 to 9% off a build
at 768 dimensions. Neither changes a bit of the graph.

**The indexes are features, and a build without one opens a file that
declares it.** `fenec-core`'s `vector`, `text`, `sparse` and `sorted` (the
four are `indexes`, on by default) are what a browser module may leave out:
`make wasm FEATURES="text sorted"`, `FEATURES=none` for none -- 180.7 KB
brotli with all four, 142.1 with none (each with the schema check), and
`make wasm-sizes` measures the sixteen sets. What stands in for a missing one is a type of no value with
the real one's methods (`off.rs`: a field of an empty enum), so the engine
compiles unchanged and the compiler drops every path through it; only the
places that make one are `cfg`'d (`Collection::new`,
`reset_index_structures`, `build_index`, the maintenance build). The file
does not change with the build: a collection declaring the index is made
and opened and its documents read and written, a `near` over it measures
every vector and a sparse `near` scores every document, as `exact` does
(below), a `match` over it is refused naming the feature (`not_built`), and
so is a `create index` of its kind -- while one replayed from the log is taken and
not built, since refusing it would refuse the file. A `@sorted` field's
comparisons and orders are the scan's, the same rows, so which types it
takes is the type's answer in both builds (`sorted::orderable`). A graph in
the file is passed over, and a checkpoint without one holds none, which a
full build rebuilds on open like a graph that does not validate. `rerank`
reads vectors out of the store and needs only `text`. The checks for a
missing index ask `EVERY_INDEX` first: the lookup of a field is a loop the
compiler cannot prove ends, and one left in cost the full module 197 bytes
brotli; asked first, `EVERY_INDEX` left it the size it had been before the
features, to the byte. A crate that depends on `fenec-core` names `indexes`
itself (the workspace takes it without default features), and `fenec-ql`
only as a dev dependency -- in its normal ones it would put them back into
every browser module.
`tests/features.rs` and the unit tests run without them in `make test`,
`web/fenec.test.js` hands files between the full module and the one
`make wasm-lite` makes, both ways, and CI runs clippy over none and each
alone.

**One module and a client entry, by where the queries run, and a test
build.** `@fenecdb/web` ships `fenec.wasm` (every index and the schema
check, 180.6 KB brotli) and `@fenecdb/web/client`, so a user has two
clear choices: queries on a server through the client entry and no
module, or a database in the page or a synced replica through the full
module. `fenec-lite.wasm` (no index, no schema check, 134.3 KB; `make
wasm-lite`, as `make wasm FEATURES=none SCHEMA=0`) was shipped beside it
and is a test build now -- built in CI, packed and released by nothing --
because it holds the invariant above: a build without an index opens a
file that declares one, and `web/fenec.test.js` hands files between it
and the full module both ways. The client entry is `web/client.js`: the builder (`builder.js`) and the HTTP client
(`http.js`) re-exported, the two modules `fenec.js` imports beside its
glue, persistence and sync. The entry's worth is that it cannot pull in
the engine, sync or storage, not the bytes: a `connect`-only app never
fetched the `.wasm` through `fenec.js` either (only `Fenec.open` does),
and bundling `connect` and a builder query's rows is 5.7 KB brotli
through the entry against 8.2 through `fenec.js` (7.5 on main before
`FenecHttp.live`), whose `Fenec` class a bundler cannot drop -- its static
block is a side effect. `web/fenec.client.test.js` walks the client's imports and fails
if they reach anything but those three files or hold the module's glue,
`FenecSync`, IndexedDB or files. `FenecHttp.live` is `Fenec.live`'s
contract over a server: a subscription to each collection the query reads
of a shape that holds nothing (`select=id&where=false`), every change a
write to it, the query run again on the server; the first rows wait for
every seed, a stream that ends is opened again and its seed runs the
query, and a text names what it reads or is refused -- so `useLiveQuery`
takes `connect()` as it takes `sync()`. A stream is a server thread
(45 to 57 KB resident) and a wake-up at every write -- a write's p50 went
0.26 -> 0.94 ms beside 60 streams and 3.1 beside 500 -- so `{ poll: ms }`
holds none: the query is sent with `If-None-Match` and the server answers
304 without running it while no collection it reads was written (`etag`,
`unchanged` in `fenec-http`, the change ring's
`changed_collections_since`; a read of rows that expire, calling `now()`
or holding an inner `get` is never tagged), 54 us against 95 for a page
of 24 run. `db.subscribe(collection, shape, onEvent, { onError, onState })`
keeps a shape over a server -- a seed, then each change -- opened again with
backoff and seeding again, stopped by a 401; `sseEvents` is exported with
it, the types beside (`ShapeFilter`, `ShapeEvent`, `SubscribeOptions`; a
sync shape was `Shape` already), and an app bundling `connect`, rows and
`live` went 7 905 -> 8 277 bytes brotli, `FenecHttp` one class a bundler
keeps whole. `db.batch([...], { idempotencyKey })` posts `/batch` -- a
builder query, a text or `[text, params]` a statement -- and a stopped one
throws `FenecError` with `at`, `status` and `completed`; `run` takes a key,
`withIdempotencyKey` makes a copy sharing `seq`, and every SDK has the same
(Go's `Error.At`, .NET's `FenecException.At`, Python's `idempotency_key=`,
the Swift, Kotlin and Dart remotes' `batch`). A replayed answer carries the
database's change as its `Fenec-Seq`, which holds the write. `Rows<P, {}>`
carries `facets?: Facets`: a `let` keeps its declared type, so a facet
asked after it was lost to the type. The client grew 6 -> 7.4 KB brotli in
an app's bundle. `FenecHttp` calls `fetch` on its
own (`#request`): as its method, a browser's `fetch` throws "Illegal
invocation", which Node's does not. `sync()` opens
the full `./fenec.wasm` unless given `wasm` or `local`. A replica's module
was shipped beside them for a while (text, sparse and sorted, no graph and
no schema check, 154.0 KB) and taken out: it left out too much for 27 KB
brotli -- no graph, so a `near` 2 to 40 times slower (0.27 -> 0.54 ms at
1 000 x 128, 0.68 -> 8.5 at 10 000 x 384, 0.98 -> 40 at 50 000 x 384), no
schema check, `create index @hnsw` refused. Without `vector`,
`near` over a field declaring `@hnsw` is `Database::near_stored`: every
vector read out of the store and measured as the full build's exact
search measures its arena (`vector::search_stored`: made a unit one as
`Arena::push` makes it, halved for f16, the same kernel, the page kept
through `best_first`, ties in id order -- the arena's when its nodes are
in id order), or over codes by `order_exactly` as the full build's exact
search over codes is, a bit index taken to hold its vectors whole while
fewer than `BIT_TRAIN` distinct ones are stored (`stored_hash`); its
tombstones, which count there, are not known here. Without `sparse`, a
sparse `near` takes the `exact` scan. `match` without `text` is refused:
BM25 needs the index's statistics. `web/fenec.test.js` holds the test
build's `near` to the full one's `near ... exact` over 9
declarations, filters and pages, row for row and score for score. The scan
costs the modules without the graph 2.2 KB brotli; in the test build at
10 000 x 128 it takes 2.4 ms against 0.22 through a graph and 0.49 for the
full module's exact scan, at 50 000 x 384 29, 0.75 and 3.9 (`make
wasm-exact-speed`): each
vector is read out of its document and made a unit one per query, four
lengths summed side by side (`flat_sqs4`; 4.3 -> 3.3 ms), where the arena
holds them made. An f16 field's vector goes into the arena as its record
holds it (`VectorIndex::as_held` in `place`): placed as written, cosine
made other unit vectors than an open did, and `near ... exact` scored
differently after a reopen.

**The browser module holds one copy of a generic where it can.** Every
type a sort, a map or a `collect` is compiled for is its own copy of the
code, and `make size-report` lists them; `WHY=<pattern>` names what pulls
one in. The sorts go through few: ids as `u64`s (`sort_unstable`),
`(DocId, f32)` by `text::best_first` -- which the text index's cursors take
too, as `(position, -ceiling)`, the stable order they had -- and rows by
`order_rows`, which a `lookup` level's order shares (its children in
ascending id first, a tie's order as before; 10 to 30% faster, its own
`(Vec<Value>, DocId)` sort gone). The vector index's candidates keep a
stable sort of their own: through `best_first` a 30 000 x 128 build took
2.5% longer, for 0.4 KB brotli. A collection's indexes by field are
`Fields`, a `Vec` searched by name, as `sorted` and `sparse` were, and the
registry's functions a `Vec` too -- each map was a copy of hashbrown, and
the registry's lowered the name into a new `String` on every call of every
row (17% of two calls a row). Against 11.2 the module lost 28.5 KB, 3.9 KB
brotli. A `str` slice that can panic keeps its panic's formatting of a
`char` -- `escape_debug` and Unicode's tables of what prints: 16.5 KB of
the module with the standard library's `to_lowercase`, `to_uppercase` and
`contains`, which slice that way. So the browser path slices with `get` and
`split_at_checked`, case is `case`'s, and `~` searches the folded bytes;
`make size-report WHY=slice_error_fail` finds a slice that brings it back.

**A vector goes out as its `f32`s.** JSON writes a vector's components
and a row's `_score` as the shortest text that reads back as the `f32`
(`json::num32_into`), as pgvector and a sparse vector's weights
do: `0.1`, where the `f64` each widens to wrote
`0.10000000149011612`. JavaScript reads it as an `f64` and a
`Float32Array` rounds that again, which gives every `f32` back but
`7.038531e-26`: as an `f64` it lands exactly between itself and the `f32`
above, and the tie goes to the even one, so it keeps its `f64`'s text
(`json::TIE`; `every_f32_reads_back_through_an_f64` tries all 2^32). The
digits of an `f32` from 2^-24 to 2^25 are exact 64-bit products
(`num::shortest`), where Ryu's multipliers took 128-bit ones that wasm
emulates (`__multi3`), and they go out in one `push_str` rather than a
`char` at a time: a page of 200 768-dim vectors as JSON, 43% shorter, went
27.7 -> 10.5 ms in the browser module, and a `near` answering ten 128-dim
rows 0.373 -> 0.226 ms (`make wasm-speed`), for 37 bytes brotli.

**A number is read three ways, each exact.** `num::parse_f64` takes
Clinger's path where the digits fit in 53 bits, divides a whole number of
up to 19 digits by `5^n` for `10^-1` to `10^-25` (`divided`; `div128`
divides in two 32-bit digits, since a `u128` division took the compiler's
own, 1.4 KB, into the browser module), and shifts decimal digits
otherwise. JavaScript writes most floats, every `f32` it widens among
them, with seventeen digits, past Clinger's 53 bits. A page of 200
768-dim vectors took 74.6 ms to read in the browser module; divided, 51.9;
with the JSON reader walking the text's bytes rather than collecting it
into `char`s first, 23.7; read where the page left it rather than copied
(`str_from`), 21.9. 10 000 128-dim vectors went in with their graph in
1 281 ms against 1 706 (`make wasm-speed`), for 368 bytes brotli.
Natively an array of numbers alone is read straight into a vector's `f32`s,
each number in the one pass that finds its end where Clinger's path takes
it (`json::clinger`) and handed to the readers above otherwise: read a
`Value` at a time, each number's text found and then read twice over, a
128-dim vector took 5.26 us, now 2.05, and 100 000 128-dim rows without an
index go in at 220k rows/s over HTTP against 123k (`make load-bench`;
213k as REST arrays, 146k as a `/batch` of single puts now). A page's
vectors come into the browser module as `f32`s already, and the reader
stays out of it: there it was 474 bytes brotli for nothing.

**A vector parameter goes into the module as `f32`s.** `web/fenec.js`'s
`run` takes each parameter that is a typed array or an array of finite
numbers -- which the module reads as a vector either way -- out of the
JSON, where it leaves `null`, and hands it over beside it: its place, its
length and its values (`vectorsApart`; `fenec_query`'s last two
arguments, `with_vectors`). A `-0` goes over as `0`, as JSON writes it,
so the file is the same either way (`web/fenec.test.js`). 200 768-dim
vectors went in in 21.9 ms as JSON and take 4.4, and `make wasm-speed`
builds 10 000 128-dim ones with their graph in 1 165 ms against 1 307.

**A quantized index holds codes, and `near` orders by the documents'
vectors.** `@hnsw(..., quant=int8)` keeps a byte a component over a scale a
vector, `quant=bit` the signs of its distance from a centre (cosine only). A
code only estimates a distance,
so `Space` (`engine.rs`) takes the beam's `ef` candidates and puts them in
order by the vectors read out of the store, as `rerank` does, so every score
is exact -- reading one only while it can still make the page. An int8 code
is off by its rounding, a step a component, and `VectorIndex::floor` is the
nearest its vector can plausibly lie (six standard deviations of that
rounding, wrong less than 1.5e-8 of the time); a candidate whose floor is past
the k-th exact distance held is neither tested nor read. At 100 000 x 768 that
is 16.8 of a beam of 100, and of 400, and 20 over a million, with recall
unchanged; a bit code bounds nothing and reads the whole beam. A filtered set under the ANN budget is
ranked by its codes and its beam's worth ordered the same way -- read whole it
was up to 12 800 vectors a query under bit codes -- and `exact` reads every
vector. Bit codes take the wider beam `BIT_EF_SEARCH` -- 200: over a million
clustered 768-dim vectors a beam of 100 held 97.9% of the true ten and 200
held 99.7% in 0.66 ms, int8 codes 97.1% at 100 (`make quant-bench`); over
the vectors' own signs the beam was 400, for 98.4% in 2.16 ms. The core settles it
wherever a spec comes in -- `VectorIndexSpec::default()` leaves `ef_search`
0 for `resolved` to fill by the codes -- since only the parser knew it once,
and the Rust API's bit indexes searched 100. How well bits estimate
depends on the vectors: spread in every dimension, 89.7% at 100 and 97.5% at
200. The code
kernels add in `strip8!`'s order on every target, so a graph over codes is the
browser's graph bit for bit; on aarch64 the int8 strips are NEON intrinsics
(`vector::neon`, as every strip there is), because the vectoriser widened
codes through a register it also accumulated in, which chained every strip to
the one before -- 5x slower, or not, depending on the code around it. A graph
over codes was record version 4, every other graph 3, so no file was rebuilt
for the feature; laid out flat (7), every record carries its quantization.

**A bit code is a residual's signs, from a centre the index learned.** The
vectors' own signs were the code before: most of a crowded cluster's signs
are its centre's, and over 100 000 x 768 spread in every dimension a beam of
100 held 36.1% of the true ten, 400 held 74.9%. A bit index learns 128
centres by k-means++ and four Lloyd rounds over its first 2 048 vectors
(`BIT_TRAIN`), holding those whole until then -- an index under that
searches exactly -- in one order on every target, so the browser learns a
server's centres; the insert that learns them takes 195 ms at 768
dimensions. A code is the signs `b` of `r = v - C`, the centre's byte, and a
word after the signs holding `σ = |r|²/|r|₁` and `κ = ⟨C, r⟩ - σ⟨b, C⟩`;
`⟨q, v⟩` is estimated as `⟨q, C⟩ + κ + σ⟨b, q⟩` -- RaBitQ's estimator
without its rotation, the query taken from the centre so that only its part
off the centre goes through the signs (`⟨q, r⟩` estimated whole held
38.7% over centres that gave 92.8%). `query_for` appends the query's
product with each centre, once a search, and a distance between two codes
counts both `κ` (`Arena::offset`), which a widened code leaves out. The same
beams hold 89.7% and 99.6%, and over 32 directions a beam of 100 went 96.8%
-> 100% at 100 000 and 82.5% -> 97.9% at a million, for 105 bytes a 768-dim
vector against 96. In an array of their own the factors were a cache miss
more a distance: the build took 69.7 s against 62.8, a query at a beam of
100 0.414 ms against 0.378. `⟨b, q⟩` is counted, not summed: `query_for`
also cuts the query to six bits a component (`QUERY_BITS`), two's
complement bit planes after the centre products, and `dot_planes` takes the
planes' popcounts under the signs -- 72 over 768 dimensions where `dot_bits`
added a float a component, whole numbers exact on every target. RaBitQ cuts
to four bits the query's distance from a centre; cut whole, a query is
mostly its own centre, and four bits held 84% of a crowded cluster's ten
against the 91.5% six hold, as the query whole does. At a million a query
at a beam of 200 went 1.30 -> 0.66 ms at 99.8% -> 99.7%, filtered 1.36 ->
0.97, the build 1 012 -> 874 s; the browser module's 0.557 -> 0.470 ms at
20 000 x 384. The planes ride in the query's `Vec<f32>` as the bits of two
floats a word, since every search hands that vector on. The centres travel in the graph record behind
quantization code 3 (`BIT_CENTRED`): a graph over the plain signs (code 2)
is built again, and a binary from before them builds its own. The browser
module grew 6.9 KB, 2.2 KB brotli, most of it the k-means.

**Filtered `near` needs its fallback.** The filter's rows are probed first -- in
blocks spread over the collection, and only until more than `ef × m0` match,
which is all the plan needs to know. A set that stays under that is searched
exactly; a larger one goes through the ANN with each candidate tested against the
filter. That second path *must* fall back to searching the whole set (the probe
carries on from where it stopped) when the result lands under the limit —
otherwise a filter correlated with the vector eliminates every candidate and
returns empty. The probe decides when rows are read, never the answer:
`tests/filtered.rs` checks it against the plan with the whole set found first.
That fallback, a set under the budget and `exact` read every vector they
measure, so natively they measure a share of 8 192 at a time on every core,
each share's nearest kept and all of those kept again in the shares' order
-- the rows, and the order of their ties, one walk in turn keeps
(`nearest_of`) -- and keep the nearest `k` as they come rather than sort
every one (`nearest`). At 1 000 000 x 128 a filter matching a quarter of the
rows, none of them near the query, took 20.8 ms and takes 5.1; `exact`
31.6 and 12.2, which is reading 512 MB. A scan asks for the vectors two
groups ahead of those it measures (`dists_to::<true>`), each a wait on
memory otherwise: 10 000 of a million 128-dim vectors in 0.41 ms against
0.82. A set an index names -- a hash bucket, the buckets of an `in` -- is
sized before it is gathered (`indexed_size`): past the budget the walk runs
with the filter as a row test, its beam widened until about twice the page
passes the filter, and where that beam would measure as many vectors as
the set holds the set is searched exactly at once, as the walk would have
ended doing. Gathered, a quarter's 250 000 ids -- copied, sorted, each
looked up -- were most of the 0.71 ms a filter keeping 25% of a million
128-dim rows took at a beam of 100, against 0.15 unfiltered, and at a beam
of 40, which a quarter passes about the page of, 43% of the walks came up
short and searched the 250 000 exactly (p99 8.9 ms); now 0.16 and 0.15 ms,
and a filter keeping 1% 0.69 against 1.22 (`make scale-bench`). The
browser module grew 2.9 KB, 1.0 KB brotli.

**Only an `and` chain reaches an index** -- equality (`=` or `in [..]`) over a
`@hash` field or over `id`, and comparisons over a `@sorted` field. `in` is a set
of equalities written short, so it is answered as the union of one bucket per
element, and only when *every* element resolves to something the field's type
can express: one it cannot sends the whole list back to the scan, because a
union missing an element's rows is a wrong answer rather than a slow one. `id`
has no `@hash` and cannot have one -- `Schema::new` reserves the name -- so the
store's id index answers it directly; before that it was a full scan, 383 us
against 0.50 us over 20 000 documents. Everything else -- `!=`, `~`, `has`, a
comparison on a field without `@sorted`, anything under `or` -- is a full scan.
`~` is unranked substring matching; ranked text retrieval is `match` over an
`@text` field, which does reach one. `order` reads every match's key but puts
only `offset + limit` rows in order -- unless it is one key over a `@sorted`
field with a `limit`, which walks the index and stops at the page; with no
`order`, the scan stops at `offset + limit` matches.

**A filter is bound to its collection once a query.** A scan, the
probe of a filtered `near`, an ordered walk and each `lookup` level test
rows through `Filter` (`engine.rs`): each field compared with a value by
its position, the value worked out once, and a row decoded in one pass for
every field the filter reads (`Store::read_fields`). What it cannot bind --
a field it does not know, a parameter not given, a call, two fields
compared -- stays an `eval` node over the stored row, so every error comes
where and as it came before: at the first row tested, and never over no
rows. Both take their comparison from one `query::compare`. Looked up by
name and cloned value by value, a scan of 20 000 rows with two comparisons
and an order took 1.91 ms natively and 2.88 in the browser module; bound,
1.07 and 1.70, and a count over an `in` and an `or` 2.24 -> 0.90 ms. The
positions are kept sorted as they come: sorted after, `usize` was a sort of
its own, 3 KB of the browser module. A row's text lands in the text its
slot held (`Store::read_fields`), a malloc and a free a row less: a count
by text equality 1.49 -> 0.96 ms natively, 1.86 -> 1.42 in the browser;
resized rather than truncated, the row cost numeric filters 7 to 13%.
Reading an `order`'s keys with the filter's own pass, rather than finding
each matched row again (`order_ids`), took 6 to 9% off an ordered scan
natively but made the unordered ones 4 to 6% slower in the browser and the
module 1.9 KB larger, so it is not in.

**`@sorted` must give the scan's answer, row for row.** Its keys order exactly
as `Value::cmp_value` orders the field's values (ints and timestamps through a
sign-bit flip, floats through order-preserving bits with `-0.0` folded onto
`0.0`), `null` is held apart below every key, and `NaN` apart from both: it
compares equal to everything, so it matches every inclusive comparison and no
strict one, and an index holding one is never walked for `order`. A literal the
key space cannot express exactly (`12.5` against an `int`, an int past 2^53
against a `timestamp`) is left out of the range and evaluated per row. Ties come
out in ascending id both ways, since that is the order the scan leaves them in
-- a descending walk reverses each run of equal keys. A walk tests the rest of
the filter row by row and gives up past an eighth of the collection, so a filter
that matches almost nothing costs 1.25x the scan rather than 2x in random reads.
The structure is a sorted `Vec` of chunks of at most 512 entries, not a
`BTreeSet`, which made the browser module 75 KB larger; `tests/sorted.rs` checks
every filter, order and page against a twin collection without the index. It is
derived data like the hash and text indexes: built when a statement first reads
it after an open (below), never in the file.

**`collate und` and `collate tr` are ICU's orders, a query's or a field's.**
The weights are ICU's own for every assigned code point --
`tools/collate/gen.py` reads ICU 76.1 out of macOS's libicucore into
`collate/`: the root cut into twelve chunks by script and each tailoring as
what it changes, with ICU's own contractions (a letter and a mark it
composes with, a two-part vowel sign, a Thai or Lao vowel written before its
consonant; up to three characters) and Hangul syllables decomposed into
jamo. ICU gives 133 535 primary weights, which no packed entry has the bits
for, so consecutive code points that alone give consecutive primaries share
a rank and are told apart by their code points (`BY_CODE_POINT`), a run of
them one range (`UNIFORM`), and a table's words are written as differences
in LEB128: 24 931 ranks, 154 KB for every script (324 KB plain), 161 KB of
`make small`'s 1331. A comparison walks ICU's three levels, letters then
accents then case, over the whole string before it falls back to the
bytes, so the order is total.
It starts at the first byte the two strings do not share, stepped back past
any character that can continue a contraction (`CONTINUES`; a fixed step is
wrong, Gurung Khema's overlap), and a Latin letter's element is made once
per collation (`Collation::latin`): 68 -> 30 ns before the root, and 33.5 ns
for `tr` and 35.2 for `und` now over a million Turkish names -- 41 while
every letter went through the tailoring's table and the root's chunk. Held
to ICU itself over 1 250 000 random pairs, five apart -- marks that compose
with their letter across another mark or out of canonical order, which
ICU's normalisation and discontiguous contractions find and fenecdb does
not -- and to `Intl.Collator` in `web/fenec.test.js`.
`order name collate und` names it for one key; a field declared `name text
collate und` has it wherever its text compares -- `order` naming none, `<`
and `>` in `where` (`RowAccess::collation`), `min` and `max`, and a
`@sorted` index over it, whose chunks order through `sorted::Entry` with the
collation passed in. Equality stays the bytes'. The schema writes a collated
field behind type tag 11 (`TAG_COLLATED`) and the collation's code (1 `tr`,
2 `und`), which an older binary refuses rather than read the field in byte
order; `\d` shows `tr-x-icu` and `und-x-icu`. A native build carries every
chunk. The browser module carries `latin` (9 KB) and is handed the rest
(`fenec_add_chunk`), which `make wasm` puts in `web/collate/`: a comparison
that meets a script it has not got notes the chunk (`collate::take_missing`)
and the statement is refused (`collate::refuse`) -- a read after it ran, a
write before it changes anything (`put`, `update`'s pass of its own,
`delete`'s filter), a load after it (`fenec_load`: its `@sorted` indexes
were built without the chunk) -- and `Fenec.query`, the builder, `restore`
and the sync layer fetch the chunks and run it again. So every stored
collated value's chunks are there, which keeps a `@sorted` index in order.
A chunk carries the tables' stamp, and another version's is refused. The
root cost the browser module 17.5 KB, 8.2 KB gzip and 6.8 KB brotli,
over the 10.4 KB `tr` and the field had; the other chunks are 145 KB, 67
KB gzip, 61 KB brotli, where plain words were 102 and 90.

**`lookup` chains, and the chain is still positional.** `lookup a ... lookup b
...` hangs `b` off `a`'s rows: what follows a `lookup` binds to *its*
collection, and `on child = parent` names a field of the level immediately
above -- so exactly one collection is in scope at any point and it is the last
one named, which is what keeps qualified names out of the language at any
depth. `Nested` holds the tree one level at a time: a level's `groups` has one
entry per row of the level above, read left to right and concatenated, so the
alignment is the same sentence at every depth and a level costs one vector
rather than a node per row. A collection may appear once per query, at any
depth -- put the driving one back in scope two levels down and `on child =
parent` reaches a level that could be either. The second level is what a
`/batch` cannot answer in one round trip: its keys live in rows that have not
come back yet. Over 2 000 / 20 000 / 200 000 for a 20 x 3 x 5 page, 49.1 us in
process against 139.8 us for the same page as 81 separate queries, and 0.204 ms
for one HTTP request against 0.415 ms for two `/batch` trips. `required` stays
a statement about its own level -- it drops rows of the level immediately above
and stops there, so it composes by being written at each level rather than by
reaching down.

**`lookup` is one bucket probe per parent, not a join.** It attaches another
collection's matching documents to the row they belong to, and a `limit` after
it counts children *per parent* -- the shape a join cannot express. It is
terminal in the grammar: everything before it binds to the driving collection,
everything after to the looked-up one, which is exactly why there are no
qualified names (`reviews.stars`) in the language and no filter splitting in
the planner. The child key must be `id` or carry `@hash`; refusing an
unindexed one follows `near` and `match`, because a silent full scan of the
child collection would be a different feature under the same name. Nesting
lives in `ResultSet.nested`, never in `Value` -- JSON transports nest all the
way down, and a flat one flattens (`ResultSet::flatten`: one row per
root-to-leaf path, nulls below the first level that ran out); a
`Value::Object` is a `json` field's value, not a level of children.
Measured at 22.7 us in process against 0.332 ms for the same
page as a `/batch` of 21 queries merged on the client.

**`required` is the other half, and it is a pass.** `lookup ... required`
drops a parent no child matches, tested before `offset` and `limit` so the page
still fills. It is also the only shape where `count` combines with `lookup`,
since nothing is being attached. There is no index from "a child matching this
filter" back to its parent, so every candidate parent is probed -- it stops at
the first child that passes, which is why the unfiltered form is an order
cheaper. It is answered from whichever side is smaller -- walk the parents probing each,
or read the children a `@hash` equality in the child filter names and take their
parents -- and the counts that decide (candidate parents, bucket length, child
collection size) are all exact, so the choice is derived rather than estimated:
child-driven when `|ids| * n > |bucket|^2`. Over 20 000 parents and 200 000
children that is 12.07 ms against 15.63 ms for the parent side, 2.19 ms with no
child filter, and 0.02 ms for a `bool @hash` field on the parent maintained on
write. Ad hoc, use `required`; on every page load, use the field.

**`match` prunes with MaxScore.** The exhaustive merge is not selective --
on BEIR FiQA the average query reaches 86% of the corpus -- so terms whose
remaining ceiling cannot beat the worst kept score stop driving the frontier.
Measured, with identical output: SciFact 179 -> 47 us, FiQA 1918 -> 311 us,
Turkish WebFAQ 401 -> 82 us. `pruning_never_changes_the_answer` compares it
against an exhaustive reference over 25 000 generated cases; scores accumulate
in `f64` so the two orders of summation agree once rounded back to `f32`.

**`@text` is word-boundary matching unless told otherwise.** `prefix=N` also
indexes each word's prefixes, which is a dictionary-free stemmer for inflected
languages: on Turkish WebFAQ it is worth +14.6% nDCG@10 for 3.4x the postings
(58 MB -> 182 MB, 82 -> 485 us per query). Off by default — the corpus
decides. The tokenizer also folds `I`, `İ`, `ı`, `i` onto one term, because the
locale-blind Unicode mapping turns `İ` into two code points nobody can type and
would hide every capitalised Turkish word. A script written without spaces
is split into runs of characters rather than words (`text::grams`): pairs of
Han, kana and Hangul, triples of Thai, Lao, Khmer and Myanmar letters --
pairs of Thai letters were too common to tell documents apart, 0.495 against
0.547 at a query three times as slow. nDCG@10 went 0.006 -> 0.439 on
C-MTEB's Chinese EcomRetrieval, 0.162 -> 0.868 on its CovidRetrieval, 0.119
-> 0.582 on the Japanese JaGovFaqs and 0.537 -> 0.682 on WebFAQ's Korean;
English scores to the fourth decimal what it did. `@text(chars)` adds each
character of Han, kana and Hangul, worth 0.439 -> 0.528 on product titles
and a little less on longer text, for 1.6 to 1.9x the postings: an index
kind of its own (7), as a quantized graph is, so a binary that knows no
`chars` refuses the file. The tokenizer takes its callback as `&mut dyn
FnMut`: generic, it was compiled six times, 8 KB of the browser module.

**The text index is derived data as well, but it is not persisted.** `@text`
builds an inverted index from the documents the first time a statement reads it
after an open — 16 µs per document against the HNSW graph's ~34 µs. Nothing
about it reaches the file, so there is no validation path and no stale-index
case to handle. It is shrunk to fit where it is known complete (its build,
`create index`); live ingest keeps `Vec` growth slack. Its term map grows a
256th at a time, as a hash index's does (`maps::Sharded`, above).

**An open builds no hash, text, ordered or sparse index.** Each is a
`Derived` -- a `OnceLock` of the index or of the error its build met -- which
an open leaves empty and the first statement that reads it fills from the
documents, under the read lock too, a second reader waiting for the first
one's build (`Collection::hash`, `text`, `sorted_index`, `sparse_index`). A
write skips an unbuilt one, since its build reads the documents as they stand
then; a block undone after a read built one takes its writes back out as it
does from any other. An ordered index over a collated field is built at the
open still: in the browser a comparison meeting a script whose chunk it lacks
has the load refused and run again with the chunk, and one built by a read
would keep the order it had without it. `stats` and `memory_bytes` count
what is built -- counted by building, every metrics scrape would build them
all. With one `@hash` field, 100 000 x 128 opens in 21.9 ms against 31.0,
and the browser module loads 20 000 x 128 with a hash, an ordered and a text
index in 19.0 ms against 41.8; the first read of each pays its build. It
cost the module 0.7 KB brotli, most of it a builder and a cell a kind.

**A server warms what an open left for the first read** (`--warm`,
`fenec_http::warm`). The first `match` after a start built its `@text`
index inside the request: a shop's first search page took 2.68 s to its
largest paint against 1.40 warm. `Database::unbuilt_indexes` lists the
derived indexes nothing has built, `warm_index` builds one as its first read
would, and a thread of the server's builds them one at a time, each under
the read lock on its own -- reads go on, one needing the index waits for
that build, a write waits out one index at most. So a bench times nothing
until its warm-up has read every index the writes keep up: `make
recon-bench`'s read all but the journal's `@hash` on its account, which no
statement there reads, and `--warm` built it -- 100 to 150 ms over a million
entries -- as the first round began, every transfer and every read behind
them waiting; a read of it waits for that build, or makes it. The thread starts once the
HTTP endpoint is up: `Server::new` takes the write lock for its watcher,
and a build's read lock held the listener back by its 141 ms. Over 100 000
products with a text, two hash and an ordered index the first answer came
16 ms after the start either way, the indexes were built 208 ms after it,
and the first `match` took 1.4 ms against 166; the process held 99 MB
against 88 for a cold one that had searched. On by default for a file (an
index is declared to be read; `--warm off`, or a list of collections and
`collection.field`s), off for a `--dir` node's tenants, which open per
request and close when idle (`Tenants::with_warm` when asked). The native
library has `fenec_warm` (and each binding a `warm`), embedded Rust
`Database::warm`.

**`rerank` deliberately uses no index.** `match ... rerank` takes candidates
from the inverted index and reorders them by exact distance over vectors read
straight out of the store — so a collection can do vector retrieval with no
HNSW graph to build, hold, validate or rebuild. Measured on BEIR it matches or
beats a full dense scan while scoring under 2% of the corpus. It requires
`match`: without candidates there is nothing to reorder.

**`fuse` adds ranks, not scores.** `match ... near ... fuse` runs both searches
to their own depth -- `candidates`, 20 unless given, never under the page --
with the filter applied to each, and a document scores `1 / (k + rank)` from
each list it is on (`k` 60). A BM25 score and a cosine distance share no
scale, and a weight between them would need retuning per corpus. Measured
with `make beir` (nDCG@10): SciFact 0.700 against 0.662 for `match` and 0.645
for `near`; FiQA 0.366 against 0.232 and 0.366 -- the one path near the top
of both. The depth is the knob that matters: up to 61 a side a document both
searches found outranks every document only one found, and at 100 a side
both scores fall (0.688, 0.359). It is built from what the engine already
had -- both searches, the `HashMap<DocId, u32>` a `DocMap` keeps sparse ids
in, the text index's `best_first` sort -- because in types of its own it was 11 KB of the
browser module; this way it is 2.

**`highlight()` reads the text again; `facet` counts beside the rows.**
`highlight(body)` and `snippet(body, N)` are select-list items (`Mark`,
its arguments one `Vec<Expr>`: Select's clone of them as fields was 432
bytes of the browser module), answered under their label, after the fields
or after `*`; the same one twice is refused, since a JSON row holds a name
once. They need `match`, and go with `fuse` and `rerank` -- a row only
`near` found has no marks. Nothing is kept in the index for them: the
row's text is split again by the tokenizer of the `@text` index the
`match` searched (`text::terms` hands each term its byte span, which the
index passes over: the build and `match` measured as before),
`highlight::Terms` holds the query's terms in the text index's own map (a
sorted `Vec<String>` was a sort of its own, 3.5 KB), and a span is marked
where a term the text yields is one of them -- so a mark is what the index
matched: a word whatever its case, the whole word a `prefix=N` term was cut
from, the characters of a Han or Thai run's matched pairs or triples,
overlapping ones merged. A span is widened over the marks of the characters
at its ends (`highlight::extends`, the blocks a text puts combining marks,
joiners, selectors and Hangul jamo in, not Unicode's whole table), and the
offsets go out as UTF-16 code units, what every client but Go's and
Python's indexes a string by. Without tags a column is `[start, end]`
pairs, a snippet's `{marks, text}`; with them the marked text, not escaped.
A snippet is the window of `N` words (a Han character a word) with the most
marks, centred on them. `facet f [top N]` counts over the rows the query
selects before `offset` and `limit` -- the filter's, `match`'s
(`TextIndex::matching`, every row holding a term, the filter tested on
those), a required `lookup`'s -- and is refused beside `near`, which ranks
every row the filter passes rather than selecting some. A `@hash` field
of text, int, bool or timestamp, whose bucket key is the value's encoding,
counts from the buckets (a bitset of the set when there is a filter); the
rest reads the field of every row, a list once a row for each value, null
a value, counted under `codec::encode_value` as `group` keys its rows, so
`tests/facets.rs` holds a scalar facet to `group ... count` over the same
filter. Ordered most first, then by value in the field's collation,
through `order_rows`; past `MAX_FACET_VALUES` (10 000) without `top`
refused, never cut. The counts live in `ResultSet::facets`, beside `nested`,
never in a `Value`, and a JSON answer carries them as `"facets"` after the
rows -- over HTTP the bare array becomes `{"rows", "facets"}` only when a
query asked for them. Over 100 000 documents a row's marks cost 1 to 3 us,
a snippet's 2 to 4; a facet over every row 0.01 ms through `@hash` and 6.1 by
the scan (`make search-bench`). The browser module grew 20.2 KB, 7.0 KB
brotli; the one without indexes 3.9 KB brotli, which counts facets and
refuses a `match` and its marks.

**A disjunctive facet's filter is split where the filter is written.**
`facet brand disjunctive` counts over the rows the query selects with the
`and` chain's terms that read the field alone left out (`Facet::rest`,
`Select::split_facets`); a term reading it beside another field is
refused, not guessed at. The split is the parser's and a REST `facet=`'s,
from the filter as written: `scoped()` and `@ttl`'s `alive` reach the
split filters through `each_filter_mut`, so a token's rules and an expiry
are never what is left out -- split after them, `facet owner disjunctive`
counted every user's rows (`tests/access.rs`). A select built in code is
split in `answered_select`, before the expiry; one that leaves nothing
out is a plain facet. Each disjunctive facet finds its own rows
(`facet_set`: `matching_ids`, `match`'s `matched_set`, a required
`lookup`) in the same statement under the same lock. `facet price ranges
[0, 2500, 5000]` counts a number by range, every range in order, value
`[from, to]` (`facet_ranges`): through a `@sorted` field's index when every
bound is a key it holds exactly (`SortedIndex::range_counts`: over every
row two searches and the chunks' lengths between, `Chunked::count`; under a
filter its entries walked against the set's bits) and by reading the field
otherwise; the lexer keeps `ranges [..]` as written, as it keeps `in
[..]`, an `f32` vector holding no bound past 2^24 exactly. Over 100 000
rows: six ranges 0.004 ms through the index against 4.7 read, 0.15
against 0.19 under a filter keeping 3%; a page with two disjunctive facets
0.75 ms in process as its three statements were, over HTTP 0.95 against
1.19 in turn. The value-count scan keeps its own loop (`each_value` serves
the ranges): through a shared one, inlined and all, it went 4.8 -> 5.2
ms, and with `facet` out of line 4.8 -> 5.6 (`#[inline(always)]`). The
two cost the browser module 2.9 KB brotli -- the split, the ranges'
read and index paths, the parser -- kept in it, so a replica or a page
answers a shop's sidebar as its server does.

**An item of a select list is an expression, and an aggregate folds
one** (`engine/aggregate.rs`). `Select::aggregate` and `computed` are
`Column`s, an expression and the name it answers under (`as`, or for a
field or an aggregate of one the text it was written as, `label_of`;
anything else is refused without `as`, since a name would have to be the
expression written back), and `group` is a list of expressions, a name of
the list standing for its item (`group_keys`). Aggregates are calls, so no
walk over an expression grew a variant: `count()` is `count(*)`,
`count(distinct e)` a call of `distinct` inside `count`, `first(e by k)` a
call of two, and `case when ... end` a call of its conditions and values,
which `eval` works out lazily; `bucket`, `greatest` and `least` are
registry functions, so `where` and `set` have them too. A key is matched to
an item by being the same field or the very item its name gives
(`is_key`), not by comparing expressions -- `PartialEq` over `Expr` was 1.5
KB of the browser module. `Reader` binds a list once a query: every field
and path read decoded in one pass into slots, `bucket` over a constant
interval and any other expression (its fields made parameters past the
query's own, `Expr::rewrite`) worked out into slots of their own once a
row, so a fold reads a `&Value` by its place. An expression over
aggregates (`sum(px * qty) / sum(qty)`) is the same rewrite over the
group's keys and folds. `count(distinct)` keeps every group's values in one
map of the hash index's type, refused past `MAX_DISTINCT_VALUES` (a
million, about 70 MB), never counted short; `first`/`last` keep the row
least/greatest by `(k, id)`, boxed so a fold stays the size it was, and
both are folded out of line (`row_held`): in the loop the fixed
aggregates took 15% longer. Natively `Fold::add` is inlined and the
paths' and expressions' reading out of line: as they first stood, a
million rows in 100 groups took 52.8 -> 54.5 ms and the whole set 32.1
-> 33.9; now 51 and 31. A `count(*)` by `bucket` of an `@sorted` or
`@ttl` timestamp or int whose filter is a range of it and nothing more
reads the index's keys and no row (`counted_by_index`, native only): a
day's per-minute counts over a million events 19.5 ms by a stored minute
field, 4.0 now. One-minute bars of a symbol for an hour out of a million
ticks, 1.7 ms where open and close were 120 queries, 195 ms (278 over
HTTP); VWAP of 50 symbols 118 ms against reading every tick out, 173;
distinct users an hour over a week 125 ms against 7.7 s (`make
analytics-bench`). The browser module grew 19.5 KB, 6.8 KB brotli, the
test build 6.6: the parser's calls and `case` (2.2 KB raw), binding and
folding (8 KB), the checks and names (1.4 KB), `bucket`'s calendar.
`site/content/docs/analytics.html` is the recipes, each FenecQL block run
by `tests/analytics_docs.rs` through `redis_docs.rs`'s runner.

**`approx_count_distinct` is HyperLogLog, and `having` keeps groups**
(`hll.rs`, `Fold::Hll`). `count(distinct)` stops at a million values, and
a day's count cannot be added to the next: Kestrel's month had 2.4 million
visitors. A sketch is 2^14 registers, a standard error of 0.81% (the root
mean square over thirty sketches of 200 000 values is 0.77%, and every
count from 1 to 2 000 000 is within 3% in `hll::tests`), read by Ertl's
improved raw estimator -- no bias tables, no switch to linear counting --
and sparse, `(index, rank)` words, until those would take a quarter of the
16 KB dense ones, so a group of a few values costs a few words. Its hash
is its own (`hll::hash`, MurmurHash3's finalizer a word), over the value's
encoding as `count(distinct)` keys it, the same on every target, so a
sketch the browser made merges with a server's. `hll_accumulate(e)`
answers the sketch as bytes (`Sketch::to_bytes`, sparse while shorter),
`hll_combine(s)` merges them and `hll_estimate(s)` (a registry function)
reads a count off one; a bytes field takes the list of numbers JSON writes
bytes as, so a sketch read out goes back in through `put $1`. A query's
sketches are held to 64 MB (`hll::MAX_BYTES`) and refused past it, as
`count(distinct)` is. The fold goes through `row_held` with the distinct
one, out of the loop the fixed aggregates fold in. Per hour over a million
events it takes 121 ms against `count(distinct)`'s 128, the furthest of 169
hours 1.3% off (`make analytics-bench`). `having <expr>` after `group`
(`Select::having`) is worked out a group as a column is, its own
aggregates folded after the list's and the list's columns read by name
(`grouped` checks it with them as keys), and a group it does not hold for
is taken back out, keys and all, before `order` and the page; `count`
after `group` counts the groups that pass. The ordered funnel -- each
visitor's first start and first finish, `having b >= a count` -- is one
row where every visitor's row left the node: 968 ms of Kestrel's month at
ten million events. The browser module grew 9.1 KB, 2.9 KB brotli, the
estimator, the sketch's bytes and `having`'s rewrite most of it.

**`sparse<N>` is pgvector's `sparsevec`, and `@inverted` answers exactly.** A
sparse vector is held as its non-zero entries, `(index, weight)` ascending
with indices from 0, and travels everywhere in pgvector's text form,
`{1:0.5,3:0.25}/N` with indices from 1 -- a JSON string, a FenecQL
literal, an imported `sparsevec` -- so a pgvector table and `fenec import`
carry the same vector.
Every way in goes through `sparse::normalise` (order, an index given twice
refused, zeros dropped), and the index relies on it. `@inverted` is the text
index's shape with weights where the counts were; `near` by dot product sums
the query's lists a list at a time into a slot a document, a window of 65 536
ids at a time (`sparse::summed`), where they are dense in the ids they span,
and walks them with MaxScore where they are not -- rank-safe, the bounds
compared once rounded to `f32` after a 1e-9 slack, so a tie-break never
depends on the pruning; the browser always sums, and has no walk. A SPLADE
query's dozens of lists reach most of a collection, and the walk, choosing a
document at a time among their heads, took 2.54 ms p50 on FiQA against 0.68
summed, 0.60 against 0.10 on SciFact; bounds per block of a list's postings
(2.69 ms) and per range of ids across the lists (443 of 451 ranges still read,
35 MB more) did not pay. `pruning_never_changes_the_answer` holds both ways to
an exhaustive walk, and `tests/sparse.rs` holds `near` to `near ... exact` row
for row. Only a document sharing a dimension with the query is ranked. Like
the text index it is derived and never persisted. It sorts through the
engine's one `(DocId, f32)` sort and maps dimensions through `DocMap`'s
`HashMap<DocId, u32>` -- its own were 12 KB of the browser module; the
feature costs 16.2 KB, 4.3 KB brotli. SPLADE++ (`beir/splade.mjs`) scores
nDCG@10 0.693 on SciFact against `match`'s 0.662, and 0.331 on FiQA against
0.232 (the dense vectors 0.366), at 0.68 ms p50 over 57 638 documents: its
queries' 37 to 65 terms reach most of the corpus. Both BEIR scripts cut texts
themselves: transformers.js drops the closing [SEP] when it truncates, SPLADE
without it took SciFact to 0.23, and the dense vectors (`embed.mjs`) moved by
up to 0.003.

**A point is two `f64`s on Redis's sphere, and `@geo` is Morton keys in the
ordered index's chunks** (`geo.rs`). A `geo` field (type tag 15) holds
`[lon, lat]` in degrees as written, 16 bytes behind value tag 15, refused
past 180 or 90 degrees rather than wrapped, and once read is a
`Value::List` of two floats: a `Value::Geo` of its own broke `Value`'s
niche, 16 -> 24 bytes on wasm32 and 6 KB more of the browser module.
`distance(a, b)` is the haversine on Redis's radius (6 372 797.56 m), so it
is `GEODIST`: over 2 000 pairs within 0.05 mm of Redis's answer from the
points it keeps (`GEOPOS`, its 52-bit geohash's cells) and 0.54 m from the
points as written; PostGIS's sphere is the mean radius, 0.03% less. Its
`sin`, `cos` and `asin` are series of `geo.rs`'s own, held to the standard
library's within a few units in the last place: the platform's libm rounds
its last bit its own way, so a point on a radius's edge could be in on a
server and out in a page, and in the browser module it brought libm's range
reduction and tables. A list of numbers going to a `geo` field, a
`distance`, a `within` or a `near` over one is read again as written
(`DataType::keeps_numbers`, `exactly_for`'s `geo_needs`), as a json
field's is: read the quick way into `f32`s, a point moved by up to 1.7 m at
the antimeridian. `within(p, [w, s, e, n])` holds the box's edges, a west
east of its east crossing the antimeridian; `near loc p` orders by
`(distance, id)`, `_score` the metres as an `f32`, `ef` refused; a build
without `sorted` answers every one of them by the scan. `@geo` (index kind
10) is derived, built at the first read as `@sorted` is: each row's key --
its longitude and latitude cut to 32 bits each, monotonely, and interleaved
-- in a `Chunked<(u64, DocId)>`, the ordered index's own chunks. A
radius's bounding box (padded 1e-7; every longitude over a pole; two boxes
across the antimeridian) or a box is covered by at most 32 cells of the
quadtree over the keys, each a range of them, each key's own cut
coordinates tested against the box with no margin, the ids left tested by
the filter; `filter_candidates` takes it by the rule a range is taken by,
while it names fewer rows than what is in hand. `near` walks the cells
best first by the nearest a cell's points can lie (`cell_floor`, a metre
and 1e-7 under), a cell ahead of a point as near so ties come out by id,
and stops at the page or at a radius the filter puts on the same point; a
hash bucket of 4 096 rows or fewer (`GEO_SET`) is measured whole instead,
and past an eighth of the collection tested the walk gives up for the
filter's set. Not a radius's own box: over 100 000 rows round cities a 50
km box held thousands of rows where the walk met a few dozen, 440 us
against 32. A row whose
latitudes alone put it past a radius (`geo::past`: the meridian arc
between them, less a metre and 1e-9 of it, which the haversine is never
under) is answered with no trigonometry, so a scan by a radius over a
million rows went 136 -> 22 ms, and `near ... exact`'s page of ten, kept
in order as the rows come rather than put in order by `order_rows`, 147 ->
6.1 ms; the test is out of `Filter::test`'s line (`geo_test`), which it
grew by a quarter. The browser module's scans run 3% slower than main's
(`make wasm-speed`'s filter 1.71 -> 1.75 ms) with no point anywhere in
their path -- the read, the test and the binding taken out, the same -- as
main's own did with 26 KB of float formatting that nothing calls: where
the code lands, not what it does. `tests/geo.rs` holds every filter,
order, page and nearest to a twin collection without the index over 2 000
rows round both poles, the antimeridian and a city, radius 0 to past half
the earth, through writes, an index made later and a block put back, and
Redis's own `GEOSEARCH` examples. Over a million points round 48 cities (`make
geo-bench`: fenecdb in process; PostGIS 3.5 and Redis 7 in Docker, each
server's own mean time read from `pg_stat_statements`, planning included,
and `INFO commandstats`): the index builds in 34 ms and holds 20 MB
(PostGIS's GiST 5.4 s and 82 MB, Redis's sorted set 86 MB); within 100 m
7.2 us p50 against Redis's 12.6 and PostGIS's 1 262 (its plan read 132
pages of the GiST); 1 km 12.6 against 34 and 1 310; 10 km, 2 543 rows,
0.56 ms against 1.73 and 4.84; 100 km, 36 341 rows, 5.9 ms against 7.6 and
32.2, the scan 22-28 ms; the ten nearest 33 us anywhere and 38 within 50
km, against Redis's 198 ms and 6.0 ms (its search sorts every member of
the cells it reads) and PostGIS's KNN 1.75 and 1.77 ms. A put with `@geo`
costs 1.19 us against 0.62 without and a `@sorted` float's 0.98. The
browser module grew 23.1 KB, 8.0 KB brotli -- the index's cover and walk
4.4 KB of the 23.1 -- and the one without indexes 11.3 KB, 4.2 brotli.

**`--follow` confirms nothing that is not on disk.** `fenec import --follow`
reads a logical replication slot through `pgoutput` and applies every change
through the copy's own mapping (`fenec-import/src/follow.rs`). The slot's
confirmed position only moves past a transaction an fsync has covered, and
every write is a put or a delete by id, so a broken stream or a killed
follower resumes from the slot and replays what it had applied without
changing it. The slot is made before the copy is read, so the stream starts
with changes the copy may already hold; the same idempotence converges them.
A `_follow` collection in the file records whether a collection's copy
finished: a copy cut short is made again rather than streamed on top of,
which would lose the rows it never reached. An update arrives without its
TOASTed columns -- a `vector(768)` is 3 KB, past the threshold -- so the
follower takes them from its pending writes or the collection; flushing
before each such read cost the batching, 5 900 rows/s against 17 100. Commit
to visible: p50 0.16 ms (`make follow-bench`). `fenec-server --follow` runs the
same follower on a thread of the server's, over the database it serves, so
the mirror is served over HTTP and subscriptions with no second process
over the file -- two corrupt it (`fenec-server/src/mirror.rs`). The
importer's options come as `--follow-index` and the rest
(`fenec_import::args`, which `fenec import` reads its own with too). The
collection takes no write but the follower thread's (a write hook, 403):
the next change from PostgreSQL would write over one, or a copy made again
forget it. The follower stops at the server's shutdown flag, and the
shutdown waits for it (`durability::before_shutdown`, 5 s at most, past which a
copy still being made is left for the next start) before its checkpoint;
an error it cannot wait out ends the process rather than leave it serving
a mirror that no longer moves. This is why the PostgreSQL client is
`fenec-wire`'s, below the importer: in the server it made the importer
depend on the server, which could then not run it. A subscriber hears a PostgreSQL
commit 0.09 ms after it returned at the median, a row inserted with a
vector under HNSW 0.19 ms, p99 4.0 and 3.3 (`make mirror-bench`; the code
before the pg wire went gave 3.7 and 3.7 the same day); a server killed
while the table was written to held every row 370 ms after it started
again. The
follower adds 178 KB to `fenec-server`.

**`/_metrics` counts at the edge, a shard per thread.** A statement is timed
around `handle`, from arrival to answer, so
the lock wait and the `--sync always` fsync are in it; whether it wrote is a
thread-local set where the write lock is taken (`metrics::wrote`), since a
connection is a thread running one statement at a time. The counters are
sixteen 128-byte-aligned shards handed to threads in turn: eight threads
counting into one set cost 720 ns a statement, 6.7 ns with the shards. The
path has an underscore because a collection may be called `metrics`.
`--metrics <addr>` is a listener for it alone that never attaches a watcher
-- a second one would take the HTTP endpoint's subscription wake-ups -- and a
tenant node publishes counts of its tenants, never a tenant's collection
names. The router counts its own the same way (`fenec-shard/src/metrics.rs`,
over `fenec_http::metrics::Timings`): requests by route and status class, a
forwarded one's time at its node, the nodes it could not reach and its moves,
never a tenant in a label -- 13.3 ns a request with eight threads counting,
beside the 17 µs the router adds. A subscription is timed to its head, and a
standby's replication stream only counted: both last as long as their
client.

**`/_stats/statements` counts by shape, a tenant's apart.** Every
statement is also counted by its text with each literal and parameter as
`$n` and a list of literals alone as one (`fenec_http::statements::shape`)
-- over HTTP by the FenecQL of `/query` and `/batch`, noted as
`api::parse_query` reads it (`statements::text`), since the strings of the
JSON would all have been one shape. The counts are shards a thread each
behind a mutex only a reader of the whole also takes, 5 000 shapes at most,
the least called forgotten: 0.34 us for a statement of 87 bytes, against the
2.2 us its parse takes (`make statements-bench`). A shape is made only as
far as the 1 000 bytes an entry keeps of it, which is as far as two can be
told apart where they are shown: shaped whole, a `put` of 1 000 128-dim
rows spent 1.7 ms of its 12.6 there. They need what reads the data without a JWT's scope, or the admin
token -- a shape names every collection -- and a tenant node keeps each
tenant's apart (`statements::View`): its own under `/t/<tenant>/`, every
tenant's, named, to the admin alone.

**A statement is parsed once, and an answer goes out in one write.**
`POST /query` keeps what `api::parse_query` parsed by the text's hash, 16
shards of at most 64, a shard emptied when full and a text over 1 KB,
which holds its literals, never kept; a JWT's scope is ANDed into a copy
(`Arc::unwrap_or_clone`), never into the shared one. An HTTP answer goes
out in one `writev`, its head written without `format!`: head and body
apart put the head in a packet of its own on a socket that sends at once,
and cost a second send each answer. One client asking a row by id over
10 000 x 128 went 40.4k -> 47.1k requests a second, and eight 14 to 20%
more (`make requests-bench`). FenecQL's lexer walks a text's bytes,
reading a character whole only outside ASCII and a number where it stands,
and a list of numbers alone becomes its vector without an expression each
(`numbers_in_brackets`): a query holding a 128-dim vector parsed in 16.9 us
and takes 6.2, a row by id 1.1 and takes 0.9. Natively the lexer reads a
list of numbers alone at once, into the vector the parser would make of it
(`Tok::Vector`, `tokenize_vectors`), each number in the one pass that finds
its end (`num::clinger`, the JSON reader's), and not after `in`, whose list
keeps its integers and `f64`s: a token a number and a comma, and each
number's text read for its end, for a `_` and for its value, a `put` of
1 000 128-dim rows parsed in 5.4 ms, now 2.4, and
`a_list_read_as_a_vector_parses_as_its_tokens_did` holds 20 000 generated
texts to the token-at-a-time parse. The browser module reads such a list
into a vector too, each number the general way: token by token, a `put`
of 1 000 768-dim rows had 39 MB of tokens, which its memory keeps. The text is sliced with
`get`: an index that can panic brought 2.7 KB brotli of a `char`'s
formatting back into the browser module, which is 1 KB smaller instead.
`the_byte_walk_reads_as_the_char_walk_did` holds the tokens, positions and
errors to the old lexer's over 40 000 generated texts.

**A plain `get` is written as JSON from the stored documents**
(`Database::query_json`). Fields alone, in the order the payload holds
them -- `select *` and most lists, with no `match`, `near`, `lookup`,
`facet`, aggregate or `count` -- are answered over HTTP by writing each
row's bytes out as JSON under the read lock, a text escaped where it lies
(`codec::text_at`), rather than decoded into a `Value` each, rendered after
the lock and dropped; anything else returns `None` and goes through `query`.
It must write byte for byte what `json::rows_array_into` writes of
`query`'s rows (`tests/query_json.rs`: every type, a field added after a
document, a dropped one, a mapped file, rows past their time). In the
container, musl's allocator gives a freed group of blocks back to the
system and faults it in again at the next request: a YCSB scan of 50 1 KB
records took 37 page faults and 213-220 us inside fenec-server, a read by
id a page mapped and unmapped while its columns were `Vec`s -- so the
columns sit in an array on the stack, and an answer's body is the
connection's spare buffer (`http::spare_body`, given back after the
`writev`, up to 1 MB): no fault, 50-60 us, and a read by id 22.4 -> 17.7
(`make roundtrip-bench`, `--measure scan50`). In process a 1 KB row took
0.95 us to decode, a field a look-up and a skip of the fields before it,
and 1.17 to write out; `select` reads a row in one pass now (`one_pass`,
0.43), `json::escape_into` tests eight bytes at a time (`needs_escape`,
0.39), and `query_json` takes 0.46 for both. The browser module's scan of
100 such rows went 0.355 -> 0.254 ms for 353 bytes brotli; `make
load-bench`'s pages read back 165.8k -> 212.0k rows a second.

**A Durable Object keeps a database as a file would** (`integrations/cloudflare`,
`@fenecdb/cloudflare`). A Worker imports a `.wasm` compiled, and `Fenec.open`
takes the module so -- it read `.instance.exports` off what `instantiate`
gives a module, and the engine could not start in a Worker at all.
`persist` writes an image into `ctx.storage` and then only the writes since
(`journal`/`drain`), until they outgrow half the image; each cut into
pieces under a key-value backed object's 128 KiB, each image under a
generation of its own and the record naming the whole one written last, so
storage that stops part way -- held to after every number of puts -- holds
the last whole database. `checkpoint` writes a new image now: a restore
takes the graphs in it, where the vectors written after the last image are
linked one after another in the browser module's one thread -- the first
`near` over 10 000 x 128 after a restart 792 ms, after a checkpoint 36
(`make cloudflare-bench`, under `wrangler dev`, which runs from the example
Worker's own `--config`: below the repository it took the site's
`wrangler.jsonc`). A Worker's isolate has 128 MB with the module's memory,
which grows and never gives back. A checkpoint writes the image a mebibyte
at a time (`fenec_snapshot_chunks`, `snapshotChunks`), each into storage
before the next is made: built into one `Vec` the image doubled as it grew,
old and new side by side, and `boxed` copied it again -- 30 000 x 128 went
from 60 to 128 MB, and goes to 70. Made all at once, the chunks stood
beside the rows until the first was stored: 50 000 x 128 at 73 MB went to
109. So each is made as it is taken (`Chunks::take`), out of the stores'
own bytes held rather than copied (`ImageOut::write_kept`, `Kept`: the
image a load kept, and each segment -- an `Arc` in the module too now,
copied by a write while an image holds it, so the image is the database
as it stood when begun): 83 MB, and a checkpoint of 10 000 x 768 holds no
more than the rows, for 0.6 KB brotli; puts and the image as fast. About
60 000 rows of 128 dimensions or 12 000 of 768 fit, 97 and 102 MB at the
most; `wrangler dev` does not hold a Worker to it.

**The browser module grows nothing by doubling past a mebibyte.** Its
memory is never given back, so a peak stays for good. An index's vectors
are `Rows` (`vector.rs`): natively a `Vec`, in the module chunks of about
`ROWS_CHUNK` (1 MiB), a whole number of rows each, reached through where
each starts (`starts`) in a load and one bounds check -- through the
chunks' own `Vec`s a 10 000 x 128 build took 11% longer, and `row` out of
line 5% (`#[inline(always)]`). A segment of documents doubles only up to
the record that seals it (`Store::append`), where it went to 12.7 MB for
8, and the lexer sets aside a token every four bytes up to 65 536 only: a
list of numbers is one token (`Tok::Vector`, in the module too now), and
a `put` of 1 000 768-dim rows had 39 MB set aside for 7 000. 5 000 x 768
written 1 000 a request held 100 MB and holds 58, written as text 130 and
62, 10 000 of them 150 and 84; a put of 200 of them as text went 10.1 ->
7.0 ms, and builds and searches as fast (`make wasm-speed`), for 768
bytes brotli. `web/fenec.test.js` holds an index over three chunks to the
exact search, before and after an image.

**A live query learns what a write changed from the change ring.**
`Fenec.live` and `FenecSync.live` share one `Lives` (`web/fenec.js`): a
query's rows now, and again once a collection it reads is written --
`Query.reads` names them, each `lookup`'s and inner `in` query's too, `null`
for a `raw` holding a `get`, and a text names them with `{collections}` or
runs after every write. Which collections were written is
`fenec_changes(since)`, `Database::changed_collections_since`: a block's
writes reach the ring as it lands, so one put back names nothing, a schema
change names its collection, and a drop answers `None` -- everything --
since a dropped collection has no name left (a name kept for each would be
kept for good). A statement's answer does not carry it: asked once in a
microtask after the task that wrote (a replica: at the next frame, a seed
landing in chunks), it costs a write nothing -- a write no live query reads
about 2 us in Node, the module 33 bytes, `make wasm-speed`'s `put` as it
was. A `load` (`restore`, `openFile`) runs every live query, since the
image's counter may be the cursor's. The client grew 1.9 KB gzip.

**An app keeps its database through `fenec-ffi`.** The engine as a native
library -- a cdylib and a staticlib, zero dependencies, its header
`include/fenec.h` written by hand and held to the exports by a test -- for
the Swift (`integrations/swift`, `Package.swift` at the root since SwiftPM
fetches a package by its repository), Kotlin (`integrations/kotlin`) and
Dart/Flutter (`integrations/dart`) bindings -- Maven Central's
`com.fenecdb:fenecdb` and `com.fenecdb:fenecdb-android`, Kotlin package
`com.fenecdb`, the JNI symbols `Java_com_fenecdb_FenecNative_*` with it.
A text is answered through
`fenec-abi`, the code the browser module answers through too -- prepare,
the exact pass, the block, the answer's JSON, the change notice -- so a page
and an app get the same bytes; out of the module's crate the optimizer at
`opt-level = "z"` kept those functions out of line (+762 bytes), and
`#[inline(always)]` left the module 161 bytes larger and 216 smaller in
brotli. A handle is a number into a table of `Arc`s, never a pointer: a use
after close finds nothing, and a call holds its database while it runs. A
read takes the shared lock -- a long one only to pin what it reads, and
reads with none held, as a server's does (`Database::pin`; a text of
several reads is one pin) -- a write the exclusive one, a lone `create
index` or `compact` runs beside the database (`Database::maintain`), and a
write's fsync runs once the lock is let go (`Database::flush`'s
`Durability`, a failure to `Database::fail`), as a server's do. Every call
runs under `catch_unwind` and returns a code -- an `Error`'s kind 1-10, or
`FENEC_PANIC`, `FENEC_MISUSE`, `FENEC_LOCKED` -- with the error's JSON, so
the library is built in the `ffi` profile, which unwinds. `<file>.lock` is
held while a file is open (a checkpoint renames a new file over the
database, which a lock on it would not survive): a second open, here or in
an app extension, is refused -- by a record lock (`fcntl`'s `F_SETLK`), the
process's, and a list of this process's lock files. An `flock` was the open
file's, which a child spawned meanwhile shares until its exec: a close and
an open again beside a thread spawning children were refused 48-66 times
in 3 000, and once in CI's `swift test`, whose servers start beside the
other tests. Every write is fsynced unless
`FENEC_OPEN_NO_SYNC`; `fenec_flush` is `Database::write_out`, the buffer
written with no fsync; `fenec_close` saves a graph once the file grew three
times its record since its last save, and syncs. The Kotlin binding's JNI
functions are the library's (feature `jni`), four entries of the JNI table
by hand, a call one `byte[]` -- its code, its JSON -- and text as UTF-8
bytes, since JNI's modified UTF-8 splits an emoji. Each binding runs its
calls off the main thread (a dispatch queue, `Dispatchers.IO`, a worker
isolate a database), sends a vector as its `f32` bytes and a json field's
`exact` refusal again as JSON, holds its builder to every golden case, and
has live queries as `Lives` has them, the looks of a burst gathered a frame
(16 ms) and taken once no write is under way. The libraries are built alone
(`cargo rustc --crate-type cdylib`): beside the staticlib and rlib, LTO left
the shared one 4% larger. 1.68 MB stripped on aarch64-apple-darwin, 1.88 on
x86_64 Linux with JNI, the sync (below) 165 and 198 KB of them; a buffered put of a 128-dim vector 4.6 us against
the server handler's 23.2, a fsynced one 4.0 ms on an M1, `near` 97.8 us
against 151.4 (`make ffi-bench`). The XCFramework is assembled by hand
(`build-xcframework.sh`), so the Command Line Tools build every slice; a
release's zip is built before its tag (`swift-binary.yml`), since the tag's
`Package.swift` must name its checksum. Swift links a static library; the
Flutter plugin vendors a dynamic `FenecFFI.xcframework` (`--dynamic`,
`FenecFFI.framework` a slice, install name `@rpath/FenecFFI.framework/...`)
and Dart opens `FenecFFI.framework/FenecFFI`: a static one kept whole with
`-force_load` had the Runner link a file CocoaPods' "Copy XCFrameworks"
phase makes with no order declared against it, and `flutter build ios`
failed on it.

**A replica's sync is a state machine; the network is the binding's.**
iOS (ATS) and Android refuse cleartext HTTP by default and a server sits
behind TLS, which fenecdb's own client does not speak and should not: the
platform's client has the trust store, proxies, pinning and power
management. So `fenec_abi::sync` does no I/O -- a binding feeds it events
(`Sync::response`, `opened`, `bytes`, `closed`, `timer`, `signal`; FFI
`fenec_sync_feed`) and performs the JSON actions it hands back (`request`,
`stream`, `cancel`, `wait`, `token`, `changed`, `refused`, `status`) with
`URLSession`, `HttpURLConnection` (Android's and the JVM's, one loop for
both) or `dart:io`'s `HttpClient`, every event handed to the core in turn on
one serial queue so a stream's bytes stay in order and no action outruns
its cancel. Porting `FenecSync` would have been three more copies of the
optimistic writes, the key reconciliation and the cursors, tested apiece;
here the logic is once, held to the scenario file (below), and each
binding's loop is a page. A write
through `fenec_query` to a shape's collection is the sync's
(`Sync::claims`, `write`): applied to the replica with what puts it back,
and queued with an `Idempotency-Key`, in one block; DDL over a synced
collection is refused, a collection with no shape is the app's own. What
`FenecSync` holds in memory is in the file, written in the blocks it
describes: `_sync_shapes` (cursors), `_sync_queue` (unanswered writes, their
keys and undo), `_sync_temps` (rows under temporary ids). Writes go one at
a time in order; 0/408/429/5xx retry with backoff (250 ms doubling to 15 s,
30% jitter), 401 asks for a token, any other 4xx is a refusal put back. A
replica reopened sends its queue before opening its streams. An update of
an unanswered insert reaches the server's copy by its key as well
(`fenec_ql::spans` keeps each statement's own text for the `/batch` lines,
an insert rendered as the builders render it, every value a parameter); an
insert the shape does not hold loses its temporary row once a stream passes
the write's `Fenec-Seq`; a seed writes over and deletes the rest, keeping
unanswered writes' rows, rather than clearing first (a row that stayed
keeps its vector's node). A change with `"schema": true`, or rows naming a
field the replica has not got, has the collection made again from `GET
/collections`, its rows kept in the fields left (`remake`), and the shape
seeded whole (`Shape::fresh`): applied as they came, the rows of an alter
failed the put and the stream retried the same change for good. A one-row change
costs 18.4 us against 7.1 for the same put alone, rows in changes of 100
3.8 us either way, a 128-dim row under HNSW 482 against 465, a seed of 10
000 rows 72 ms against 44 (`make sync-bench`). It adds about 165 KB to the
native library -- 1.51 -> 1.68 MB on aarch64-apple-darwin, 1.68 -> 1.88 on
x86_64 Linux, about 77 KB of it the sync's own code -- and the browser module is built without it:
`fenec-abi`'s `sync` feature, which only `fenec-ffi` turns on. Compiled
in as dead code, it still moved LLVM's inlining and left the module 155
bytes larger; off, the module is the size it was, 512 400 bytes. Dart's is
`Fenec.openSynced`, since a static `sync` cannot sit beside the instance's
fsync `sync()`.

**The browser's sync and the native core are held to one scenario file.**
Moving `FenecSync` onto `fenec_abi::sync` was measured at +21 KB brotli of
the browser module, so the two are written apart, and
`integrations/sync-scenarios.json` says what both do: 60 scenarios, each a
script of shapes, app writes and server events -- a seed, a change, a
stream dropped, the status each write's request is answered with, a seed
past the horizon, the network's signal, a token -- with what the replica,
the queue, the requests sent (method, path, body, the idempotency key
reused or new), the timers and the refusals are after every step.
`crates/fenec-abi/tests/scenarios.rs` drives the sans-IO core through it,
and `web/fenec.sync.scenarios.test.js` drives `FenecSync` through a fetch
and a clock the script plays, its replica persisted over an in-memory
IndexedDB so a restart is a page opened again; each writes the names that
passed to `target/sync-scenarios/`, and `make sync-scenarios-check` (in
`make test` where the module is built, and its own CI step) fails unless
both ran every one -- under `CI` the JS runner fails rather than skip
without `web/fenec.wasm`. A behaviour of either changes in the file first.
A step's values are the builders' texts, so a write's fields are written
in alphabetical order (the native runner's JSON sorts them, JavaScript
keeps them as given); `<name>` binds a value made at run time -- a key, an
`Idempotency-Key` -- and `@name` is a fixture. Where the platforms differ
on purpose a scenario says why in `differs` and a step holds
`expect_js`/`expect_native`; none does now. What is left between them is
the platforms': a JS write's promise settles with the server's answer
where a native write returns once kept, JS has `batch()` where a native
text of several statements is one write, tabs elect a leader, and the
browser fetches collation chunks. Bringing the browser to the core fixed
what it got wrong: a network failure or a 5xx put a write back, an update
or a delete of an unanswered insert went by the temporary id the server
never saw, an insert the shape did not hold kept its row until the next
seed, a seed cleared the collection and every pending row with it, a
write made offline was gone with the page, and each tab sent its own
writes. Its state is now the core's, in the replica: `_sync_shapes`,
`_sync_queue` and `_sync_temps`, stored the moment a write is made
(`persist`'s journal, or the file of an `openFile`d `local`); a follower
tab applies its write and hands it to the leader, which keeps it in its
own queue and tells every tab the answer, and a tab taking the lead reads
the queue its predecessor stored (`#lead`, through a `sibling` database).
`fenec-http`'s CORS allows `Idempotency-Key` and exposes `Fenec-Seq`,
without which a page on another origin could not send the one or read the
other. `fenec.js` grew 113.3 -> 132.5 KB, 31.1 -> 35.8 KB brotli -- 3.1 KB
of it code, the rest the comments -- and the browser module did not change
by a byte.

**A schema declared in code is checked at every open, in the engine.**
Two front ends compile to one description (`fenec_core::declared`,
versioned JSON: `{"format": 1, "collections": [...] | "fenecql": "...",
"migrations": [...]}`): tables written Drizzle 1.0's way in TypeScript
(`web/schema.js`, its own entry point `@fenecdb/web/schema`, so a page that
declares nothing loads none of it), and FenecQL `create collection` text --
a `schema.fenecql` -- which every SDK hands over with no builder of its own
(`fenec_ql::schema_text`). `declared::plan` compares a database's schemas
with it: what only adds (a collection, a field, an index where there is
none, a path index) comes back as statements; a field the code lacks
(dropped, or renamed? never guessed), a type, a collation or `required`
changed, an index changed or taken off, a required field new to a
collection that exists, are refusals with their resolution; a collection
the code does not declare is left alone. `Mode::Follow` is for a database
another owns -- a replica's server, a client without the right -- where
the code's must be there and the rest is the owner's. `fenec_abi::schema`
runs the migrations not yet recorded (known by their place in the list,
held to their text: one edited after it ran, or a database holding more
than the code lists, is refused), records each in `_migrations (n, text,
at)`, plans and applies the additions, all one block, put back on any
refusal or error; a database holding none of its own collections is made
from the description and records its migrations without running them. A
plan runs the pending migrations in a block put back. `rebuild` is the
migration that changes an index or a collation: the field added again as
declared, `set` from the old, the old dropped, the new renamed. The module
exports `fenec_schema` (feature `schema`, on by default, `make wasm
SCHEMA=0` without; reading the collections' JSON was 8 KB of it, so the
module reads FenecQL alone), the native library `fenec_schema` (plan,
apply, follow, describe), the server `GET /_schema`, `POST /_schema/plan`
(`?mode=follow` for a scoped token) and `/_schema/apply` (the server's
token, under the write lock). `Fenec.open`, `restore` and `openFile`
apply; `sync` follows (the server owns a replica's schema); `connect`
follows, or applies with `migrate: true`; Python's, Go's and .NET's
`schema(...)`, Swift's `Fenec.schema(...)`, Kotlin's and Dart's
`schema(...)` (JNI's `schema`, Dart's worker op) take the text. A `load`
adds an image's collections to what a database holds, so one that holds
only what its schema made, and nothing since, is emptied before a restore
or an openFile loads into it (`untouched`). `integrations/schema-golden.json`
holds declarations, texts and plans (`make schema-golden` writes it
through the module; `fenec-abi`'s tests run every case natively, the JSON
and the FenecQL form of each). The check costs an open of 10 collections
0.09 ms in Node and the module 7.5 KB brotli (8.1 until the plan's two
modes were one const generic and its messages one helper each); `fenec.js` grew 1.8 KB
brotli for the hooks and the check's errors, and `schema.js` is 5.9. `fenec types` reads a
`.fenecql` as it reads a database, and writes Python, Go, C#, Swift,
Kotlin and Dart rows (`--lang`; Kotlin's and Dart's tests compile the
golden file and read a row into it), the tables as code (`--schema`) and the schema as FenecQL
(`--fenecql`); `integrations/types-golden/` holds each for one schema.

**`integrations/` may use outside packages; the crates may not.** The
LangChain and LlamaIndex vector stores (`integrations/python`, one package,
the standard library for its client) and `useLiveQuery`
(`integrations/react`) are held to their frameworks' own tests, and each
language's docs example to a real server (`integrations/languages`: the
same flow over HTTP in Python, JavaScript, Go, C#, Java, PHP, Ruby and
Rust, each with its own HTTP client) -- `make python-test` runs LangChain's
standard suite and the tests LlamaIndex's integrations run from a
`python:3.13` container against a fenec-server started here, `make
languages-test` the eight examples (Python, Java, PHP and Ruby from their
images, the others with the toolchain on the machine), `make react-test`
runs the hook against a real replica, and CI runs all three
(`integrations`). The Go and .NET SDKs (`integrations/go`, module
`github.com/fenecdb/fenec/integrations/go`, package `fenecdb`;
`integrations/dotnet`, NuGet's `FenecDb`, `net8.0`) are the standard
library alone, the Python client's surface -- a statement and its rows,
typed rows, writes with their `Fenec-Seq`, `/batch`, an idempotency key,
`After` for a replica, a subscription, `/_changes`, a tenant, `/_health`,
a typed error -- and write a `float32` as the shortest text that reads back
as it through an `f64`, which is how the server reads a number, but for
`json::TIE`, which goes as its `f64`'s text: `encoding/json` wrote its
shortest `f32` text and the server stored the float above. A request is
bounded through its context or token, never the HTTP client's timeout,
which would cut a subscription short. `make go-test` and `make
dotnet-test` start the servers they need (`FENEC_SERVER`, else
`target/debug/fenec-server`), CI runs both, and `make languages-test`
runs the docs' Go and C# examples through each SDK too (`go-sdk`,
`dotnet-sdk`). Python, Go and .NET have the JS builder too
(`fenecdb/builder.py`'s `db.collection(...)`, Go's `db.From(...)` and
.NET's `db.From(...)`), and all four make the same text of the same chain,
to the byte, refusals by the same message: `integrations/builder-golden.json`
holds the chains (`{op, args}`, the JS builder's names, what JSON has no
word for an object of one `$` key) and what the JS builder made of each --
`web/golden.mjs` holds the chains and writes the file (`make
builder-golden`), `web/fenec.test.js` runs every case through the JS
builder again so the file cannot drift, and `make python-test`, `go-test`
and `dotnet-test` run every case through their own builder, the
endpoints' statements read off a recording transport. A builder change
starts in `web/fenec.js` and a case in `golden.mjs`, then the three
follow. Go's object conditions are `Fields`/`Ops` and a document a
`D(...)`, names and values in turn, since a map has no order and the order
is the text's; its options are functional, since a struct cannot tell
`limit 0` from none. CI also builds the three packages as a release
publishes them and installs and uses them (`integrations/packages.sh`):
PyPI's `fenecdb`, npm's `@fenecdb/web` -- the client, both modules and
`collate/`, `web/package.json` -- and `@fenecdb/react`. They go out when a
release's draft is published (`packages.yml`), only after that same check,
with a token where the registry's secret holds one (`NPM_TOKEN`,
`PYPI_API_TOKEN`), with the workflow's OIDC identity (trusted publishing)
where it does not (RELEASING.md). NuGet's `FenecDb` is packed, installed
into a fresh console app and used against a server
(`integrations/dotnet/package.sh`) and only then pushed with
`NUGET_API_KEY`, its job skipped with a notice without the secret; the Go
module has no registry, and `release.yml` pushes the tag
`integrations/go/vX.Y.Z` it is fetched by beside `vX.Y.Z`. With
`full_text=True` a store indexes its text for BM25 as well and searches by
the words (`match`) or by the words and the vector fused (`fuse`):
LlamaIndex's `TEXT_SEARCH` and `HYBRID`, LangChain's `mode="text"` and
`"hybrid"`. `alpha` is not read, since `fuse` adds ranks; `quant` passes
`int8` or `bit` codes to `@hnsw`. The same store for LangChain.js
(`integrations/langchain`, `@fenecdb/langchain`) runs over anything with
`run(sql, params)` -- a `Fenec` keeps a RAG index in the page -- and is
held, LangChain.js publishing no standard suite, to what its own vector
store integrations are tested for, over a database in the page and over a
fenec-server's HTTP endpoint (`make langchain-test`); the Vercel AI SDK has no
store interface, so `integrations/ai-sdk/rag.js` is three functions to copy,
held to the SDK's mock models (`make ai-sdk-test`). `web/fenec.d.ts` is
held to `web/fenec.js` by `make types-check`: every export and class method
read at run time and touched from a generated file, and a caller's code with
the lines the types must refuse, under `tsc --strict` -- the module stays
JS with no build step. A store names
its collection and metadata columns in the statement's text, so both are
checked against FenecQL's name pattern; values always go in as parameters,
and `in` takes one per element (`in [$2, $3]`), since a parameter binds a
value and not a list.

**Profiles differ on purpose.** `fenec-cli` uses the `cli` profile (`panic =
abort`, single process, nothing to recover). `fenec-server` stays on `release`: a
panicking connection thread unwinds and drops only its own request. A cold
crate is built for size in the profile it is cold in: in `cli`, `fenec-http`, which the shell reaches only for
`backup`, `archive` and `restore`, waiting on the network and the disk
(16.3 KB of the released binary). Not the importer, whose SQLite import of
200 000 rows took 285 ms against 251 for 32.6 KB, nor the root crate, which
built for size made the binary 129 KB larger (`make size-report BIN=fenec`).
Both binaries hold 72 KB of the standard library's backtrace symbolizer
(`gimli`, `addr2line`, `rustc_demangle`, `object`), which only a nightly
`build-std` leaves out.

## Conventions

- `Error` (`fenec-core/src/error.rs`) is the single error type: allocation-free
  variants, no `Box`. `fenec-http` maps it onto HTTP statuses (`api::status_of`).
- Unit tests live inline in `#[cfg(test)] mod tests`; cross-crate and protocol
  tests live in `crates/*/tests/`, a file each, gathered into one binary a
  crate by `tests/all.rs` (`autotests = false`: a new file needs its `mod`
  line there, or it is never built). One that reads the process's own
  counts, `/_metrics` or the statements', is a `[[test]]` of its own:
  beside the others it would count theirs. Measurement programs are
  `crates/fenec-core/examples/` and are wired to `make` targets, not to CI.
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
- Comments explain *why* a thing is the way it is — a measured cost, a trap that
  was hit, an alternative that was rejected. Match that when adding code.
- All prose in the repo (comments, docs, README) is English.
