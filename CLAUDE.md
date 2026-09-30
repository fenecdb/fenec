# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

fenecdb — a minimal, vector-native embedded database in Rust. Compiles to WASM for
the browser, has its own query language (FenecQL) and speaks the PostgreSQL v3 wire
protocol. The docs under `site/content/docs/` are the long-form reference (design
rationale, benchmarks, full FenecQL and HTTP surface); `README.md` is the
front door and links into them, and this file is the working summary.

## Commands

```bash
make test          # cargo test (no fenec-bench, no examples), fenec-core without its indexes, then the JS tests
make wasm          # builds fenec-wasm for wasm32, copies to web/fenec.wasm
make wasm FEATURES="text sorted"   # without the other indexes (FEATURES=none: none of them)
make wasm-lite     # the module without any, to web/fenec-lite.wasm (web/fenec.test.js)
make wasm-sizes    # the module's size with each of the 16 sets of indexes
make size-report   # where the module's bytes go, by crate, module and std (BASE=main: against main; BIN=fenec-pg: a native binary's)
make wasm-speed    # the module in Node: HNSW build, near, filter, match, JSON (speed.mjs a.wasm b.wasm compares builds)
make packages      # fenecdb (PyPI), @fenecdb/web and @fenecdb/react (npm) as a release publishes them, installed and used
make version V=X.Y.Z   # one version wherever a release reads it (RELEASING.md)
make serve         # wasm + python3 http.server -> http://localhost:8787
make bench         # scale measurement (fenec-core/examples/bench.rs)
make memory        # memory footprint, for calibrating --max-memory
make sweep         # ef / recall trade-off
make compare       # vs SQLite + pgvector (needs `make pgvector-up` first)
make python-test   # LangChain + LlamaIndex stores vs their frameworks' tests (Docker)
make drivers-test  # psycopg, asyncpg, SQLAlchemy (Docker), pgx, node-postgres, tokio-postgres over the pg wire, pgvector's library for each
make react-test    # useLiveQuery vs a real fenec-pg replica (needs `make wasm`)
make beir BEIR=dir # nDCG@10 per ranking path (vectors: crates/fenec-bench/beir, embed.mjs + splade.mjs; BM25 alone without; FENECBENCH_TEXT=chars sets @text's options)
make import-test   # the PostgreSQL arm of import and --follow (needs Docker)
make follow-bench  # --follow: commit-to-visible latency, drain, reconnect (pgvector-up first)
make mirror-bench  # fenec-pg --follow: commit to a subscriber, a server killed and started again
make small         # smallest `fenec` binary: --profile cli --no-default-features
make pg PGPASS=secret HTTP=127.0.0.1:8080   # run the server against ./data.fenec
make node ADMIN=secret   # a tenant node: fenec-pg --dir tenants (PG=addr adds the pg wire)
make shard               # the router in front of the nodes (./shard.fenec)
make shard-bench         # router overhead per request, tenant move time, failovers by hand and on a lease
make replica-bench       # replica lag per sync policy, catch-up, what a failover loses
make tx-bench            # a pg transaction: a lone write per sync policy, a write in one of 100, in a savepoint
make concurrency-bench   # writers and readers at once against SQLite: durable and buffered writes, reads beside a held transaction
make requests-bench      # a request over the pg wire and HTTP: one client's round trip, eight's rate, against PostgreSQL
make load-bench          # loading 100 000 rows each way a client can send them, against PostgreSQL's COPY and INSERT
make maintenance-bench   # reads and writes during create index / compact
make open-bench          # opening a 1 GB file, read into memory or mapped
make reopen-bench        # a crashed 100k x 768 file: linked at the open, beside the queries, or with its graphs kept
make quant-bench         # quant=int8|bit against full vectors: memory, recall, latency
make scale-bench         # fenec-pg against pgvector over the pg wire at scale: load, memory, recall, latency, filters (pgvector-up first)
make statements-bench    # what counting a statement by its shape costs
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

`make test` runs Rust first on purpose: `cargo test` builds the `fenec-pg` binary
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
fenec-ql    (lexer + parser)          fenec-wasm  (C ABI, core+ql)
     |                                fenec-catalog (pg_catalog SQL, core only)
fenec-http  (REST/JSON + SSE, tenant registry, replication, /_metrics)
     |                     \
fenec-wire  (pg wire:        fenec-shard (tenant router: directory,
     |      framing, client)              placement, move)
fenec-import (SQLite file reader + PG COPY source + --follow)
     |
fenec-pg    (wire protocol server, catalog from fenec-catalog,
     |      --follow running the importer's follower)
fenec-cli   (`fenec` shell, `fenec import`, `fenec types`)
```

`fenec-bench` is a measurement harness (`publish = false`) and is the only crate
allowed external crates — that is where `rusqlite`/`postgres` live.

`fenec-core` modules: `store` (segments, offset index), `engine` (`Database`,
`Collection`, replay/snapshot/compact/checkpoint), `vector` (HNSW + distance
kernels), `text` (tokenizer, inverted index, BM25), `query` (`Statement`, plan
execution), `schema`, `value`, `codec`, `collate` (ICU's root order and its Turkish
tailoring, generated tables in chunks),
`json`, `num` (decimal text to `f64` and back), `case` (a string's Unicode
case, as the standard library's, without its code), `time` (calendar arithmetic), `sparse`
(sparse vectors and their inverted index), `changes`
(the change ring), `plugin` (registry), `fs` (buffered file I/O, behind the
`std-fs` feature), `off` (what stands in for an index a build is made
without).

The browser client is `web/fenec.js` — WASM glue (~352 lines), the query builder,
the HTTP client and the sync layer, in one dependency-free ES module. `web/fenec.d.ts`
holds the types; `fenec types <file>` generates schema-specific declarations.
`persist`/`restore` keep a database in IndexedDB as a file would hold it: an
image, then the writes since as chunks, from the journal `fenec_journal` starts
and `fenec_drain` empties (off until asked for -- a page that never drains would
hold every write). One row persists in 0.18 ms over 32 MB in Chrome, against 92
ms for the image. `openFile` (a dedicated worker) keeps the same bytes as a file
of the origin private file system -- the file `fenec-pg` keeps, so each opens
the other's -- and `run` appends each statement's writes and flushes before it
answers: 0.56 ms a row, statement included, and it opens in 20 ms against
IndexedDB's 47 (`make file-bench`; Safari 0.98 and 0.36 ms). A new image
(`compact`, or appended writes past half the image and 64 KB) goes into
`<name>~` and is flushed before the file is written over, so a crash leaves
one of the two whole; `openFile` takes the copy when its image is whole.

## Invariants worth knowing before you change things

**Zero dependencies is a hard rule** for `fenec-core`, `fenec-ql`, `fenec-wasm`,
`fenec-http`, `fenec-wire`, `fenec-pg`, `fenec-import`, `fenec-shard`, `fenec-catalog`. The WASM output has to stay small and
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
machine, which read it cannot. `fs::open_in_memory` (`fenec-pg --no-mmap`,
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
`HANDOVER_AT` (16 MB) or 65 536 records, `Database::hand_over` has the sink
write what is pending (`Sink::written_through`) and each store point the
documents it holds at their places in the file and let its segments go
(`Store::hand_over`): 820 MB after that load. Where each record went is
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
(`landed_block`), which a replica applies whole. A savepoint stops the
spills (its marks are where the stores stood in memory), and a block that
spilled is not parked -- written again, its writes would be read into
memory again -- so readers wait for it. With them the peaks were 904 and
888 MB (in process, in a `rust:alpine` container), and the one block
without a graph loaded in 3.8 s against 8.6. A binary from before refuses
kinds 10 and 11. The browser module writes no spill, and reading them cost
it 1.5 KB, 0.5 KB brotli.

**Single writer, and readers beside it.** Reads take a shared lock
(`Database::query`), writes the exclusive one (`execute_with`). A pg
transaction is a block held open (`fenec-pg/src/server.rs`, `Hold`) from
its first write -- its first statement under `SERIALIZABLE` -- until
`COMMIT` lands it or `ROLLBACK`, a failed statement (`25P02` after it, as
PostgreSQL), the client's going or `--idle-in-transaction-timeout` (`25P03`,
10 s) puts it back. The session takes the write lock for each of its
statements alone (`Turn`) and leaves the block open between them
(`Database::leave_block`): every other write waits for the transaction to
end, which is the isolation -- nobody writes after a block's writes, since
a block is put back by cutting each store back to where it stood -- and a
read goes on. A reader that finds a left block's writes in the database
parks it (`Database::park`): the rollback's own undo, a write at a time,
the frames kept, and the owner's next statement writes them again first
(`unpark`, the write paths' upkeep with no hook and nothing to the sink),
so readers read what has landed and nothing else, exactly. A block that
changed a graph -- undone, a node becomes a tombstone, and written again
another node -- or the schema is not parked, and readers wait for it as
they did for every block. The engine refuses a statement run through
`&mut self` into another session's left block, and a `query` of one not
parked, rather than join it or show its writes; `fenec_http::held` is how
every path in the servers takes the database -- `read_landed`,
`write_unheld`, `read_landed_now` for the graph keeper, which passes over
a block rather than park it or wait for it, and `read_quiet` for a
tenant's export, which waits for the transaction to end so that its
commit lands before the move. A `Hold`
puts its block back when dropped, however the session ends. The hold
keeps a database found once a pass of the session loop (a `OnceCell` a
pass), so a tenant's is found afresh for the next transaction. A pipeline of the extended protocol is one
block up to its `Sync` when a write in it has more of the pipeline after
it; the last statement before a `Sync` runs as one on its own. A create,
a drop and a create index are writes of the block, put back with it; a
compact rewrites the file, so it runs on its own before a transaction's
first write and is refused after one (`25001`). A lone `create index`
outside a transaction is still built beside the database.
`SAVEPOINT` takes the held block's `Database::savepoint` -- the start of
the block before the first write -- and `ROLLBACK TO` puts back what came
after it (`Database::rollback_to`) and goes on, a failed transaction too:
a savepoint after a write keeps the failed block and the lock
(`TxState::keeps`), as would one in a serializable transaction; one before
every write lets both go, and taken back to, the lock as well. `COMMIT
PREPARED` is refused rather than read as the `COMMIT` it begins with, and
`ROLLBACK TRANSACTION TO s` was once read as a `ROLLBACK`. A simple
query's text holding transaction control, or anything else `compat`
answers, runs a statement at a time (`compat::statements` splits it,
quotes and comments kept whole; a text with no `;` is not walked for
one, which a `put` of 1 000 128-dim rows spent 1.8 ms of its 12.6 on) through the pipeline's path: the
statements outside a transaction take its implicit hold, which a `BEGIN`
makes the transaction's, a `COMMIT` lands and the text's end lands, and
the first error ends the text -- as PostgreSQL's implicit block. Read
whole, `BEGIN ; put ...` was taken for its `BEGIN`, the rest dropped; a
text of FenecQL alone still runs whole, one block, and the extended
protocol refuses several commands (`42601`). Under `--sync always` a transaction lands with one
fsync: a put in one of 100 costs 90 us against 3.97 ms alone, and a lone
statement what it did (`make tx-bench`). A savepoint costs its round trips:
a put in one of its own, released, 58.3 us against 20.7 under `--sync 250`,
and a `ROLLBACK TO` over 100 writes 30.9 us. What one writer costs readers
is measured against SQLite in one process (`make concurrency-bench`): four
threads read 8.0M rows/s by id alone, 707k beside four writers, and 8.1M
beside a transaction held open 20 ms at a time, the longest read 0.3 ms --
held under the write lock throughout, the transaction let them read 272k
and kept one waiting 34 ms; SQLite's WAL reads the last commit meanwhile,
1.03M/s and 0.7 ms at most. Durable writes gain from the fsync outside the
lock: 253 -> 537 writes/s from 1 to 16 writers, SQLite's 270 -> 270. Two processes opening the same file corrupts it,
which is why `fenec-http` is a second listener inside `fenec-pg`, never
its own binary.

**`COPY FROM STDIN` is one block, put 10 000 rows at a time**
(`fenec-pg/src/copy.rs`, `server::copy_in`). psql's `\copy`, psycopg's
`copy` and JDBC's `CopyManager` send it as a simple query, tokio-postgres
through Execute with a Sync behind it, which means nothing until the
CopyDone -- PostgreSQL ignores a Sync during a COPY as well. The rows go in
as puts of 10 000, or of 32 MB of text, each linking its vectors on every
core: the first with more to come takes the block and holds the lock
between messages as a transaction does, its waits for the client bounded as
a transaction's; a simple query's block lands at the CopyDone, an
Execute's at its Sync. An error, a bad row, a cancel or the client's
CopyFail is answered at once and puts every row back, and the session loop
drops what the client still streams, as PostgreSQL does. The COPY counts
as one statement however many puts it made (`run_copy`). Text, CSV and
PostgreSQL's binary format, each binary cell read by the type its column
is described as (`copy::binary`): asyncpg's `copy_records_to_table` and
pgx's `CopyFrom` ask `SELECT the columns FROM the table` for the types
first, which `sql::select` reads as the `get` it is. 100 000 rows x 128: 17.6k rows/s with the graph kept and
171k without, against 17.2k and 152k a put a row in a transaction, and
PostgreSQL's own COPY 434 and 62k (`make load-bench`, the median of three;
`site/content/docs/benchmarks.html#loading` has every way).

**`COPY ... TO STDOUT` reads a page at a time** (`server::copy_out`).
psql's `\copy ... to`, psycopg's `copy`, asyncpg's `copy_from_table` and
`copy_from_query`, pgx's `CopyTo` and tokio-postgres's `copy_out` read a
table out this way, and it was refused. A collection's rows go in id order,
1 000 at a time by `get <c> select .. where id > $last limit 1000`, each
page under a read lock of its own and held against a move for its read
alone, then written to the client with no lock held: read whole under one,
a slow client kept every writer waiting and every row was in memory. So a
row goes out once, as it stood when its page was read, and a transaction
that holds the database reads it as it stands throughout. A page costs only
its own rows because a full scan starts at the floor an `id > x` or `id >=
x` in the `and` chain sets (`Expr::conjunct_id_floor`,
`Store::iter_ids_from`): a million rows a page at a time took 8.6 s, now
110 ms; native only, since it was 0.4 KB brotli of the browser module. A
cell is its PostgreSQL text escaped as `CopyAttributeOutText` or quoted as
`CopyAttributeOutCSV` would -- `COPY FROM` reads the row back -- or, in
binary, what a binary query's row sends, the `PGCOPY` header riding in the
first row's CopyData: psycopg reads a message a row, and took the header
alone for a row cut short. `COPY (get ...) TO STDOUT` runs the one `get`
whole, as on its own, its `_score` and `lookup` levels in the row. 100 000
rows x 128 go out at 157k rows/s in text and 1.54M in binary, against
PostgreSQL's 156k and 747k (`make load-bench`).

**A driver reads rows and sends parameters in the formats it asks for.**
A column goes in binary where Bind's result codes ask (`binary.rs`: the
types' `typsend`, a text type as its text, an array refused), which
tokio-postgres and asyncpg ask for every column and pgx for every type it
knows -- sent as text, a `bigint` was one byte where they read eight. A
parameter's type is the one its place names (`params.rs`: the field it is
given for or compared with, `id`'s, a `near`'s field's, a `match`'s text),
text where nothing names one, and never the unspecified OID 0, which sent
tokio-postgres into a type lookup that recursed until its stack ran out and
asyncpg into an introspection query of its own; Bind reads a binary value by
it. A value sent as text is read as the field its place names
(`params::places`), as COPY reads a cell of it, and by its look -- a
number, a boolean, a vector, text -- only where no field is named, or it
is no value of the field's: by its look alone, `"t"` was a boolean and
`"42"` a number, which a text field refused. psycopg and node-postgres
name no type for a string and send `Parse` to `Execute` in one go, so a
statement no `Describe` asked about has its places found at its first
Bind, under the read lock. A vector is pgvector's type -- `vector`, `halfvec` for a `vector<N,
f16>`, `sparsevec`, 16400 to 16402 as the catalog names them (`pg_oid`) --
in pgvector's binary formats both ways (`binary::vector`): described as
`text`, it was a string to pgvector's clients, which register their codecs
by the type's name. A vector not in the format is read as the text it went
as before, which holds no zero byte where the format's second word is 0,
and one holding a NaN or an infinity is refused (`22000`), as pgvector
refuses it. A page of 1 000 768-dim vectors reads in 5.5 ms against 31.4 as
text (tokio-postgres, the rows not decoded). A list is PostgreSQL's
array of its element's type (`binary::array_of`: `[text]` is `text[]`,
`[int]` `bigint[]`), in `array_out`'s text and `array_send`'s binary form,
and a parameter in its place is described so: as `text`, every driver read
`{a,b}` as a string. A list of lists or of vectors has no array and stays
text.
A plain `SELECT` of columns from one collection is the `get` it is
(`sql.rs`), which asyncpg and pgx ask before a binary COPY. Inside `COPY
(...) TO STDOUT` it also takes the conditions DuckDB's postgres extension
pushes down (`sql::plain`), their literals read as the columns' types --
DuckDB quotes an int's `100` as `'100'` -- a column cast to text sent as
its text, and the `ctid` range covering every row, part of one refused:
fenecdb has no row addresses. Outside a COPY it takes Spark's JDBC reads:
each condition in parentheses, a number bare and text quoted and so read
(`Plain::inline`), `WHERE 1=0` for the columns, and `SELECT 1 FROM t` for
`count()`, answered with as many rows of the constant as rows match
(`constant_rows`) -- a row's id in the constant's place would have counted
the same and been a wrong answer. DuckDB opens with `SELECT version(), (SELECT
COUNT(*) FROM pg_settings ...)`, which `compat` answered as `version()`
alone, dropping the column DuckDB then could not read; a call is answered
there only alone now, and anything beside it goes to the catalog. A new field type
needs its binary form in both. `make drivers-test` holds psycopg,
SQLAlchemy, asyncpg, pgx, tokio-postgres and node-postgres to their own
flows, and pgvector's library for each: pgvector-python over psycopg and
asyncpg, pgvector-go, pgvector-node and pgvector-rust.

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
(`Statement::fits_block`), and a `/batch` or a pg text holding one runs
each statement on its own. A schema change is undone as a write is: the
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
log rather than popping it was 0.5 KB more. A `/batch`, a pg text
of several statements, a pg transaction and pipeline, and the browser
module's `run` of several are one block. A block is put back a write at
a time, the last first, each the inverse of what it did: 3.0 ms for 50 000
writes, where finding each id's first write by searching took 402 ms, and
a sort was 4.6 KB of the browser module. A `Savepoint` is how far the block's buffers
reached and each written store's `Mark`, and `rollback_to` puts back what
follows it the same way. The buffers are kept from one block to the next (`Block::cleared`),
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
`fenec-pg` under `--sync always` reports it (`58030`) instead of the success it
had not yet sent. That fsync runs *outside* the exclusive lock: under it a
write only calls `Database::flush`, which hands back a `Durability` to run once
the lock is released, and `FileSink` writes the bytes there as well (a `write`
under the lock waited out concurrent fsyncs on macOS). A durability whose bytes
an earlier fsync already covered runs none, which is the group commit: 268 ->
1 156 durable writes/s over eight clients. A failed one is reported back with
`Database::fail` so the engine stops taking writes. The syncer of `--sync
<ms>` flushes the same way, a database's and each of a node's tenants'
(`Tenants::sync_dirty`): the tenants' held their lock through the fsync, and
every read and write of one waited up to 27 ms a pass on macOS.

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
following database refuses writes (`Error::ReadOnly`, `25006`). Lag is 0.20 ms
p50 under `--sync always` and at most 283 ms under `--sync 250`; ten failovers
under `always` lost no acknowledged write. An archive (`fenec archive`,
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

**Scaling out is by tenant, one file each** (`fenec-pg --dir`, `fenec-shard`,
whose directory replicates to a standby router like any other file, the
standby's maps catching up from the change ring -- 4 us a change at 100 000
tenants against 38 ms reading them all again under the router's write lock;
`site/content/docs/sharding.html`). The tenant comes from the path
(`/t/<tenant>/`), or over the pg wire from the startup packet's database
(`--listen` in `--dir` mode; looked up again per statement, so a move, an idle
close or a delete between two of them is seen), never from the query, so tenants cannot share a file -- they
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
lease (`fenec-shard --auto-failover`, nodes `fenec-pg --dir --lease`;
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
deletion" would hand every user everyone's ids. The algorithm is the server's
(HS256 only), never the token's.

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
each of its writes: a data record, or a create's, a drop's or an index's.
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

**`make scale-bench` holds fenec-pg to pgvector over the wire.** The same
client asks both -- the `postgres` crate with pgvector-rust's types -- with
the same vectors and m and ef_construction, and each side takes its best
way: fenec-pg keeps its graph as a binary COPY lands, pgvector builds after
it in memory on every core (`parallel_workers`: 768-dim vectors live in
TOAST, and PostgreSQL planned one process for them), reads its index into
its buffers and searches a filter with `iterative_scan`; the container
needs 2 GB of shared memory for that build (`pgvector-up`). At a million
128-dim vectors fenec-pg loads and indexes in 47.2 s against 117.8, on
612 MB of disk against 1 432, holding 721 MB against 1 132 -- on Linux for
both: the engine's count, which is the anonymous memory the same load held
in a container, against the container's anonymous memory and the shared
memory of its buffers, read from its cgroup in a container started afresh
(`pg_memory`: the buffers keep what an earlier run left); the pages of
their files each keeps besides are counted apart -- answers at a beam of
100 with 99.1% recall in
0.147 ms against 98.3% in 2.35, a filter keeping 1% in 0.63 ms against
13.5, and eight clients at 17 001 queries/s against 2 097
(`site/content/docs/benchmarks.html#scale`). The laptop this was measured
on is a fanless M1 Air, which slows to a third under minutes of load on
every core: a comparison runs its sides in turns, each after idle minutes,
and a figure from a hot run is not one.

**A block's `put`s link their vectors together.** A block
`Database::begin` opened -- a pg transaction or pipeline, a `/batch`, a
COPY -- is its statements' batch: a `put` in it leaves its vectors waiting
(`defer_batch`, `Block::waiting`), and they are linked on every core 512
at a time a field (`LINK_AT`, `link_waiting`), the rest as the block lands,
before its record is written, so that a block put back after all is put
back as any other. A search in the block measures the waiting ones
exactly, as it does a server's backlog, and a rollback or a `ROLLBACK TO`
drops those its undo made tombstones (`forget_waiting`) -- the newest of
the waiting, since nothing else leaves one while a block is open. A lone
statement links as it did. Linked a row at a time, as a driver's
`executemany` and a `/batch` of single puts send them, 100 000 128-dim rows
went in at 5.4k rows/s with the graph kept, 5.3k over HTTP; linked
together, at 16.9k and 16.0k, as a COPY loads them (`make load-bench`).

**A server answers before its graph is linked.** A server checkpoints only
on its way down, so a crash after a long run leaves every vector written
since in the tail, and linking them at the open kept the port closed for as
long as they took: at 100 000 x 768 never checkpointed, `fenec-pg` answered
its first `near` 67.7 s after it started, and linking at the open still
takes 18.4 s with the lists pruned in parallel. `fenec-pg`, a tenant and a replica
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
while the graph is written). A database with a block open is passed over
until the next look: a block that changed a graph cannot be parked, and a
COPY of 100 000 128-dim vectors with the graph kept held the keeper 3.0 to
4.3 s, and every other database it keeps with it. The record holds the whole graph, so a bound
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
`fenec-pg --cdc`; a primary's feed with `--replication-token`). A
subscription keeps a query's rows and is reseeded past its ring of ids,
which holds no documents; change data capture has to see every write once.
So it reads the records the feed keeps for replicas (`Feed::changes_after`:
from the record holding the write after the cursor, where a replica is sent
an image) and has the engine read them out a write at a time
(`Database::changes_in`, native only): numbered as the counter numbered
them, each document by its collection's schema as the database knows it or
as a create among the records made it. `since` is the last write a
consumer has and `Fenec-Next` the last one an answer holds; a cursor inside
a block's record goes on after the write it names. Only what an fsync
covered is handed over, a cursor the feed no longer reaches is answered 410
with the first `since` it does -- never with writes missing -- one past the
last write 409, and `wait` waits on the feed's `Condvar` for a write. A
scoped token is refused: its filter could not hold back the deletion of a
row it never saw. A consumer with no state of its own has the server keep
where it is (`/_changes/consumers/<name>`, rows of `_consumers`, made by a
`POST` at the last write on disk -- read from "now" each time, it missed
what came between two reads -- and moved by a `POST` of `since`), each
write at least once; the stream leaves `_consumers`' own writes out, the
cursor going past them. A feed kept for it alone has no token (`Replication`'s
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
Kept for `--idempotency-ttl` (a day), let go of a range of the `@sorted`
`at` at a time, at most once a minute; the change stream leaves them out.
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

**The reader writes nothing** (`fenec-pg --reader <name>`). The pg wire
had one password, which wrote. A session whose startup user is the
reader's name is authenticated against a password of its own (SCRAM picks
its verifier by the name before the proof) and carries `TxState::reader`,
kept through `begin` and `end`: every write is refused (`25006`) before a
compact or an index is built beside the database, `COPY FROM` before a row
is read, whatever the session sets. It needs `--password`, and the two
passwords must differ.

**`insert` never writes over; `put` does.** `Statement::Put` carries
`insert` (the parser sets it for the keyword, `POST /<name>` over REST):
a document naming an id the store holds is `Error::Duplicate` -- 409 over
HTTP, `23505` over the pg wire, where `Exists` is a collection's `42P07`
-- and the statement, a block of one, is put back whole. JWT scoping keeps
the flag as it rewrites the statement; made a `put` there, a scoped insert
would write over. The JS builder's `.insert()` still sends `put`: the sync
layer writes rows back through it when it undoes an optimistic write.

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
was. Graph records 9 and 10 carry the aliases after the tombstones; a
restore checks each alias's document holds the node's vector. Tests whose
data repeated vectors (`i % 13`) to count nodes and tombstones now write a
vector a row. The browser module 2.2 KB brotli; builds, searches and
opens as fast.

**Limits error, they do not truncate.** `near` results cap at 10 000 rows
(`limit + offset`), expression depth at 512 levels and a `lookup` chain at 8;
all three return a query error, because a silently cut result is a wrong answer
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
`make wasm FEATURES="text sorted"`, `FEATURES=none` for none -- 138.1 KB
brotli with all four, 106.1 with none, and `make wasm-sizes` measures the
sixteen sets. What stands in for a missing one is a type of no value with
the real one's methods (`off.rs`: a field of an empty enum), so the engine
compiles unchanged and the compiler drops every path through it; only the
places that make one are `cfg`'d (`Collection::new`,
`reset_index_structures`, `build_index`, the maintenance build). The file
does not change with the build: a collection declaring the index is made
and opened and its documents read and written, `near`, `match` and a sparse
`near` over it are refused naming the feature (`not_built`), and so is a
`create index` of its kind -- while one replayed from the log is taken and
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
(`json::num32_into`), as the pg wire, pgvector and a sparse vector's
weights do: `0.1`, where the `f64` each widens to wrote
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
index go in at 220k rows/s over HTTP against 123k, 184k by COPY against
110k, and 160k by `executemany` against 105k (`make load-bench`). A page's
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
`make small`'s 1040. A comparison walks ICU's three levels, letters then
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
way down, the PostgreSQL wire flattens (`ResultSet::flatten`: one row per
root-to-leaf path, nulls below the first level that ran out), and the "no
nested objects" rule stands. Measured at 22.7 us in process against 0.332 ms for the same
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
`create index`); live ingest keeps `Vec` growth slack.

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

**`sparse<N>` is pgvector's `sparsevec`, and `@inverted` answers exactly.** A
sparse vector is held as its non-zero entries, `(index, weight)` ascending
with indices from 0, and travels everywhere in pgvector's text form,
`{1:0.5,3:0.25}/N` with indices from 1 -- a JSON string, pg text, a FenecQL
literal -- or in its binary form where a pg driver asks for it, so a
pgvector client and `fenec import` carry the same vector.
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
to visible: p50 0.32 ms (`make follow-bench`). `fenec-pg --follow` runs the
same follower on a thread of the server's, over the database it serves, so
the mirror is served over the pg wire, HTTP and subscriptions with no second
process over the file -- two corrupt it (`fenec-pg/src/mirror.rs`). The
importer's options come as `--follow-index` and the rest
(`fenec_import::args`, which `fenec import` reads its own with too). The
collection takes no write but the follower thread's (a write hook, 25006):
the next change from PostgreSQL would write over one, or a copy made again
forget it. The follower stops at the server's shutdown flag, and the
shutdown waits for it (`server::before_shutdown`, 5 s at most, past which a
copy still being made is left for the next start) before its checkpoint;
an error it cannot wait out ends the process rather than leave it serving
a mirror that no longer moves. This is why the pg wire's framing and client
are `fenec-wire`'s: in fenec-pg they made the importer depend on the
server, which could then not run it. A subscriber hears a PostgreSQL
commit 0.15 ms after it returned at the median, a row inserted with a
vector under HNSW 0.33 ms (`make mirror-bench`); a server killed while the
table was written to held every row 880 ms after it started again. The
follower adds 178 KB to `fenec-pg`.

**The catalog is run, not matched.** psql's `\d`, JDBC's `DatabaseMetaData`
and DBeaver send SQL over `pg_catalog` -- joins, `CASE`, `regclass` casts,
correlated subqueries, `UNION`, window and set-returning functions -- and the
texts change with every client version, so `fenec-catalog` evaluates that SQL
over tables made from the schemas each time rather than pattern matching it.
A collection is a table in `public` with `id` its primary key; fenecdb's
indexes are indexes with their own access methods (`hash`, `btree` for
`@sorted`, `hnsw`, `bm25`). Joins find rows by key where the query names an
equality: over a thousand collections nested loops took JDBC's column lookup
18.1 s, keyed 122 ms. What the subset cannot read, and catalog tables it does
not build, answer empty -- the old behaviour -- so a tool never stalls. A
`WITH` is read, and a recursive one runs as PostgreSQL runs it, its first
select and then each `UNION` over the rows the step before added, until a
step adds none, 1 000 steps at most (`with_rel`): asyncpg looks up a type it
has no codec for with one, `WITH RECURSIVE` over a derived table and again
in a scalar subquery. A parameter is the type the query casts it to (`param_types`: asyncpg's
`$1::oid[]` arrives as the binary array it is told), or else the type of
the column it is compared with: tokio-postgres looks a type up by `t.oid =
$1` and binds the oid only to a parameter described as one. `to_regtype`,
`bit`, `pg_range`'s columns and pgvector's types in `public` are there
because pgvector's clients look types up by them -- asyncpg's
`set_type_codec` in the schema it is given, where `sparsevec` in
`pg_catalog` was not found. It is
a crate of its own so that it can be built for size (`opt-level = "z"`): at
opt-level 3 it added 390 KB to the amd64 image, built for size 295 KB, for
queries 1.2-1.5x slower. The CLI and the browser module link none of it.

**`/_metrics` counts at the edge, a shard per thread.** A statement is timed
in `execute_into` (pg) and around `handle` (HTTP), from arrival to answer, so
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
rows spent 1.7 ms of its 12.6 there. Over the pg wire they are
`pg_stat_statements` (a `fenec-catalog` table, filled only for a query that
names it). They need what reads the data without a JWT's scope, or the admin
token -- a shape names every collection -- and a tenant node keeps each
tenant's apart (`statements::View`): its own under `/t/<tenant>/` and over
its connection, every tenant's, named, to the admin alone.

**A statement is parsed once where the protocol lets it be.** A driver
prepares a statement (`Parse`) to bind and run it again and again, so
fenec-pg reads its text as FenecQL there and keeps it with the statement
(`Prepared::parsed`); `Describe` and `Execute` take it, and a text `compat`
answers, or one that does not parse, is taken at `Execute` as before.
`POST /query` has no statements to prepare, so `api::parse_query` keeps
what it parsed by the text's hash, 16 shards of at most 64, a shard
emptied when full and a text over 1 KB, which holds its literals, never
kept; a JWT's scope is ANDed into a copy (`Arc::unwrap_or_clone`), never
into the shared one. An HTTP answer goes out in one `writev`, its head
written without `format!`: head and body apart put the head in a packet
of its own on a socket that sends at once, and cost a second send each
answer. One client asking a row by id, over 10 000 x 128 (`make
requests-bench`): the extended protocol 48.0k -> 52.0k requests a second,
HTTP 40.4k -> 47.1k, and HTTP 14 to 20% more with eight clients; the
parser was a quarter of what the pg wire did for one and half of what
HTTP did. Where a text is parsed, FenecQL's lexer walks its bytes, reading
a character whole only outside ASCII and a number where it stands, and a
list of numbers alone becomes its vector without an expression each
(`numbers_in_brackets`): a query holding a 128-dim vector parsed in 16.9 us
and takes 6.2, a row by id 1.1 and takes 0.9, and the simple protocol's
`near` went 10.5k -> 11.6k requests a second. Natively the lexer reads a
list of numbers alone at once, into the vector the parser would make of it
(`Tok::Vector`, `tokenize_vectors`), each number in the one pass that finds
its end (`num::clinger`, the JSON reader's), and not after `in`, whose list
keeps its integers and `f64`s: a token a number and a comma, and each
number's text read for its end, for a `_` and for its value, a `put` of
1 000 128-dim rows parsed in 5.4 ms, now 2.4, and
`a_list_read_as_a_vector_parses_as_its_tokens_did` holds 20 000 generated
texts to the token-at-a-time parse. The message's text is checked as UTF-8
whole before `from_utf8_lossy` walks it in chunks (`take_cstr`). A simple
`put` of 1 000 went in at 105k rows/s without an index, and at 180k now,
past `COPY` (`make load-bench`). The browser module reads such a list
into a vector too, each number the general way: token by token, a `put`
of 1 000 768-dim rows had 39 MB of tokens, which its memory keeps. The text is sliced with
`get`: an index that can panic brought 2.7 KB brotli of a `char`'s
formatting back into the browser module, which is 1 KB smaller instead.
`the_byte_walk_reads_as_the_char_walk_did` holds the tokens, positions and
errors to the old lexer's over 40 000 generated texts.

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

**`integrations/` may use outside packages; the crates may not.** The
LangChain and LlamaIndex vector stores (`integrations/python`, one package,
the standard library for its client) and `useLiveQuery`
(`integrations/react`) are held to their frameworks' own tests, and the pg
wire to what psycopg and SQLAlchemy send (`integrations/drivers`: a nested
transaction is a savepoint to both) -- `make python-test` runs LangChain's
standard suite and the tests LlamaIndex's integrations run from a
`python:3.13` container against a fenec-pg started here, `make
drivers-test` the drivers' nested transactions and psycopg's COPY the same way,
asyncpg's typed parameters and rows, and pgx's, node-postgres's and
tokio-postgres's with the Go, Node and Rust on the machine, pgvector's
library for each beside them, `make
react-test` runs the hook against a real replica, and CI runs all three
(`integrations`). CI also builds the three packages as a release
publishes them and installs and uses them (`integrations/packages.sh`):
PyPI's `fenecdb`, npm's `@fenecdb/web` -- the client, both modules and
`collate/`, `web/package.json` -- and `@fenecdb/react`. They go out when a
release's draft is published (`packages.yml`), only after that same check,
with a token where the registry's secret holds one (`NPM_TOKEN`,
`PYPI_API_TOKEN`), with the workflow's OIDC identity (trusted publishing)
where it does not (RELEASING.md). With
`full_text=True` a store indexes its text for BM25 as well and searches by
the words (`match`) or by the words and the vector fused (`fuse`):
LlamaIndex's `TEXT_SEARCH` and `HYBRID`, LangChain's `mode="text"` and
`"hybrid"`. `alpha` is not read, since `fuse` adds ranks; `quant` passes
`int8` or `bit` codes to `@hnsw`. The same store for LangChain.js
(`integrations/langchain`, `@fenecdb/langchain`) runs over anything with
`run(sql, params)` -- a `Fenec` keeps a RAG index in the page -- and is
held, LangChain.js publishing no standard suite, to what its own vector
store integrations are tested for, over a database in the page and over a
fenec-pg's HTTP endpoint (`make langchain-test`); the Vercel AI SDK has no
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
abort`, single process, nothing to recover). `fenec-pg` stays on `release`: a
panicking connection thread unwinds and drops only its own session. A cold
crate is built for size in the profile it is cold in: the catalog in
`release`, and in `cli` `fenec-http`, which the shell reaches only for
`backup`, `archive` and `restore`, waiting on the network and the disk
(16.3 KB of the released binary). Not the importer, whose SQLite import of
200 000 rows took 285 ms against 251 for 32.6 KB, nor the root crate, which
built for size made the binary 129 KB larger (`make size-report BIN=fenec`).
Both binaries hold 72 KB of the standard library's backtrace symbolizer
(`gimli`, `addr2line`, `rustc_demangle`, `object`), which only a nightly
`build-std` leaves out.

## Conventions

- `Error` (`fenec-core/src/error.rs`) is the single error type: allocation-free
  variants, no `Box`. `fenec-pg` maps it onto PostgreSQL SQLSTATE codes.
- Unit tests live inline in `#[cfg(test)] mod tests`; cross-crate and protocol
  tests live in `crates/*/tests/`, a file each, gathered into one binary a
  crate by `tests/all.rs` (`autotests = false`: a new file needs its `mod`
  line there, or it is never built). One that reads the process's own
  counts, `/_metrics` or the statements', is a `[[test]]` of its own:
  beside the others it would count theirs. Measurement programs are
  `crates/fenec-core/examples/` and are wired to `make` targets, not to CI.
- Comments explain *why* a thing is the way it is — a measured cost, a trap that
  was hit, an alternative that was rejected. Match that when adding code.
- All prose in the repo (comments, docs, README) is English.
