# AGENTS.md

This file provides guidance to Codex (Codex.ai/code) when working with code in this repository.

fenecdb — a minimal, vector-native embedded database in Rust. Compiles to WASM for
the browser, has its own query language (FenecQL) and speaks the PostgreSQL v3 wire
protocol. The docs under `site/content/docs/` are the long-form reference (design
rationale, benchmarks, full FenecQL and HTTP surface); `README.md` is the
front door and links into them, and this file is the working summary.

## Commands

```bash
make test          # cargo test, fenec-core without its indexes, then the JS tests (node --test)
make wasm          # builds fenec-wasm for wasm32, copies to web/fenec.wasm
make wasm FEATURES="text sorted"   # without the other indexes (FEATURES=none: none of them)
make wasm-lite     # the module without any, to web/fenec-lite.wasm (web/fenec.test.js)
make wasm-sizes    # the module's size with each of the 16 sets of indexes
make serve         # wasm + python3 http.server -> http://localhost:8787
make bench         # scale measurement (fenec-core/examples/bench.rs)
make memory        # memory footprint, for calibrating --max-memory
make sweep         # ef / recall trade-off
make compare       # vs SQLite + pgvector (needs `make pgvector-up` first)
make python-test   # LangChain + LlamaIndex stores vs their frameworks' tests (Docker)
make react-test    # useLiveQuery vs a real fenec-pg replica (needs `make wasm`)
make beir BEIR=dir # nDCG@10 per ranking path (vectors: crates/fenec-bench/beir, embed.mjs + splade.mjs; BM25 alone without; FENECBENCH_TEXT=chars sets @text's options)
make import-test   # the PostgreSQL arm of import and --follow (needs Docker)
make follow-bench  # --follow: commit-to-visible latency, drain, reconnect (pgvector-up first)
make replica-bench # replica lag per sync policy, catch-up, what a failover loses
make maintenance-bench # reads and writes during create index / compact
make open-bench # opening a 1 GB file, read into memory or mapped
make quant-bench # quant=int8|bit against full vectors: memory, recall, latency
make statements-bench # what counting a statement by its shape costs
make small         # smallest `fenec` binary: --profile cli --no-default-features
make pg PGPASS=secret HTTP=127.0.0.1:8080   # run the server against ./data.fenec
```

Single tests:

```bash
cargo test -p fenec-core --test persist          # one integration test file
cargo test -p fenec-ql near                      # by name substring (integration: fn name only)
cargo test -p fenec-core codec::tests            # inline unit tests in a module
cargo test -p fenec-core --no-default-features --features std-fs --lib --test features   # without the indexes
cargo test -p fenec-import --test pg -- --ignored   # needs a live PostgreSQL
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
fenec-http  (REST/JSON + SSE, replication, /_metrics)
     |
fenec-pg    (wire protocol: server AND client, catalog from fenec-catalog)
     |
fenec-import (SQLite file reader + PG COPY source + --follow)
     |
fenec-cli   (`fenec` shell, `fenec import`, `fenec types`)
```

`fenec-bench` is a measurement harness (`publish = false`) and is the only crate
allowed external crates — that is where `rusqlite`/`postgres` live.

`fenec-core` modules: `store` (segments, offset index), `engine` (`Database`,
`Collection`, replay/snapshot/compact/checkpoint), `vector` (HNSW + distance
kernels), `text` (tokenizer, inverted index, BM25), `query` (`Statement`, plan
execution), `schema`, `value`, `codec`, `collate` (ICU's root order and its Turkish
tailoring, generated tables in chunks),
`json`, `num` (decimal text to `f64`), `time` (calendar arithmetic), `sparse`
(sparse vectors and their inverted index), `changes`
(the change ring), `plugin` (registry), `fs` (buffered file I/O, behind the
`std-fs` feature), `off` (what stands in for an index a build is made
without).

The browser client is `web/fenec.js` — WASM glue (~305 lines), the query builder,
the HTTP client and the sync layer, in one dependency-free ES module. `web/fenec.d.ts`
holds the types; `fenec types <file>` generates schema-specific declarations.
`persist`/`restore` keep a database in IndexedDB as a file would hold it: an
image, then the writes since as chunks, from the journal `fenec_journal` starts
and `fenec_drain` empties (off until asked for -- a page that never drains would
hold every write). One row persists in 0.6 ms over 32 MB, against 136 ms for the
image.

## Invariants worth knowing before you change things

**Zero dependencies is a hard rule** for `fenec-core`, `fenec-ql`, `fenec-wasm`,
`fenec-http`, `fenec-pg`, `fenec-import`, `fenec-catalog`. The WASM output has to stay small and
auditable; own codec, own JSON, own HNSW, own SCRAM/crypto, own decimal-to-`f64`
(`str::parse` drags in a 12 KB table -- see `num.rs`). `fenec-core` does
dev-depend on `fenec-ql` (Cargo allows the cycle through a dev dependency) so tests
can write real queries.

**No page cache, and the file is mapped.** The byte sequence on disk and in
memory are the same format, so a read decodes straight over the bytes: no
eviction policy, no dirty pages, no cache of fenecdb's own. `fs::open` maps
the file where the target can, so the documents stay in it and the process
holds what it derived from them (1 GB file, hash and ordered index: 188 MB
against 1 095 read in; `compact` peaks at 236 MB against 2 012).
`fs::open_in_memory` (`fenec-pg --no-mmap`, replicated files and `--dir`
tenants included) is the other way. A rewrite points the stores at the file
it just wrote (`Database::repoint`) from where it put each record, without
reading the file back (1.8-2.3 s of a 1 GB checkpoint, now 19 ms; an
adopted image is still walked), and a compact over a mapped file copies no
record -- on a server either -- and rebuilds only a graph holding
tombstones.

**Single writer.** Reads take a shared lock (`Database::query`), writes the
exclusive one (`execute_with`). There are no transactions — `fenec-pg` accepts
`BEGIN`/`COMMIT` and does nothing with them, and refuses (`0A000`) a `ROLLBACK`
that follows a write in the block rather than answering "done". Two processes
opening the same file corrupts it, which is why `fenec-http` is a second
listener inside `fenec-pg`, never its own binary.

**A storage error stops writes.** Once the sink refuses an append, a sync or a
rewrite, every later write and sync returns `Error::Io` until the file is
reopened (`Database::failure`); reads go on, from memory and the mapped
file's pages -- a page that cannot be read in is `SIGBUS`, not an error, so
`--no-mmap` for failing disks, and a mapped file is replaced only by rename. A failed `fsync` is
never retried -- the kernel may already have dropped the pages -- and
`fenec-pg` under `--sync always` reports it (`58030`) instead of the success it
had not yet sent. That fsync runs *outside* the exclusive lock: under it a
write only calls `Database::flush`, which hands back a `Durability` to run once
the lock is released, and `FileSink` writes the bytes there as well (a `write`
under the lock waited out concurrent fsyncs on macOS). A durability whose bytes
an earlier fsync already covered runs none, which is the group commit: 268 ->
1 156 durable writes/s over eight clients. A failed one is reported back with
`Database::fail` so the engine stops taking writes.

**Replication ships only what is on disk, numbered by the change counter.**
A primary (`--replication-token`) writes through a `Tee`
(`fenec-http/src/replication.rs`): a write's record enters a bounded feed in
memory as it is appended, and is sent once an fsync has covered it -- so a
primary back from a crash holds every write any replica was sent. A replica
applies them with `Database::apply`, which does the write path's index upkeep
and hands each record on to its own sink with `Sink::record(seq, ..)`: its
change counter matches the primary's write for write, and its file reopens
where it stopped. So every write goes through `wal`, one record and one tick;
a record that moved no counter would leave every replica one change off. The
history (record kind 8, `History`) moves none, as a graph a server keeps in
the tail does, and neither is sent. A promotion forks the history, a replica is continued only from a position
the primary's history passed through and sent an image otherwise, and a
following database refuses writes (`Error::ReadOnly`, `25006`). Lag is 0.20 ms
p50 under `--sync always` and at most 283 ms under `--sync 250`; ten failovers
under `always` lost no acknowledged write. An archive (`fenec archive`,
`fenec-http/src/archive.rs`) is the same stream written to files, each write
with the time the primary appended it; `fenec restore` is an image plus the
archived writes up to a time or a change, forked -- a fenecdb file is exactly
that, so a restore is a concatenation checked by opening it.

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
128 reads waited at most 21 ms through an HNSW build and 69 ms through a compact
(file rewrite included), against the full ~20 s under the write lock. Over a
mapped file a compact copies no record: graphs holding tombstones are rebuilt
beside it, the live records streamed into the new file under the lock. Only a
lone statement takes this path; a batch, the shell and `execute` hold the lock.

**File format** (see README *File format*): every record is
`[kind][collection-id][length][body]`. The length is written even for an empty
body and the reader **must** consume it, or the stray byte is read as the next
record kind. A last record a crash cut short is cut off the file on open
(`Database::load` says where, `fs::open` and `replication::open` cut): only
skipped, it swallowed the next append, an acknowledged write lost on the open
after. A record cut short inside the checkpoint image is refused as corrupt
instead (an image is renamed in whole), and `fenec types` opens with
`fs::open_read_only`, which writes nothing to a server's live file. The change counter record (kind 6) is at the front and fixed width;
the id counter (kind 7) exists so `compact` cannot hand out a deleted id again;
the history (kind 8) and a graph a server keeps in the tail (kind 4) are the
appended records that are not writes.

**The HNSW graph is derived data, not a cache.** It is written by
`snapshot`, `compact` and `checkpoint`, and by a server into its file's tail
(below) — never on the write path. On open the
version, dimension, precision and link bounds are validated, and the live nodes
against the documents holding a vector; anything off means a silent full
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
without that, one `del` rebuilt the graph on every open until `compact`, which
rebuilds a graph holding tombstones (nothing else takes one out). A rewrite
touches only the indexes whose field it changes, so an update of a title no
longer takes the vector out of the graph and back in (1.89 -> 0.006 ms at
20 000 x 768, and no tombstone); "unchanged" is to the bit. An unfiltered
`near` the tombstones cut short walks again with the beam wider by their
number, or searches exactly where that walk costs more than reading every
vector (`past_tombstones`); without it a `limit 10` answered 4 rows.

**A server keeps its graphs in its file.** It checkpoints only on its way
down, so a crash after a long run left every vector written since the start
to link again. `fenec_http::link::keep` looks at every database the process
serves every 5 s, and appends to the tail each graph that changed in 10 000
nodes since it last reached the file, has none waiting to be linked, and
whose record the file has grown three times over since
(`Database::save_graphs`, under the read lock, which keeps the writes out
while the graph is written). The record holds the whole graph, so a bound
on the linking alone would have a big graph written over and over for a
little of it; waiting for the linking kept a crash from leaving the nodes
waiting again. It is a graph record (kind 4) of version 6, which an older
binary does not know and rebuilds from -- it restored a record in the tail
against the documents the whole file left, and a vector rewritten after it
kept the links of the one before -- and like the history it moves no
counter and no replica is sent it (`Tee::append`). At 100 000 x 768 a crash
leaves at most 10 000 vectors to link, 7.9 s with `near` at 2.06 ms p50
meanwhile, where every vector waited 55.2 s in the same run with `near` at
the exact scan's 14.89 ms; the ten records were 35.2 MB of a 370.3 MB file
until the next checkpoint, and each held the read lock 15.0 ms p50, 27.1 ms
at most (`make reopen-bench`).

**Limits error, they do not truncate.** `near` results cap at 10 000 rows
(`limit + offset`) and expression depth at 512 levels; both return a query error,
because a silently cut result is a wrong answer believed right. Full table in
README *Fixed limits*.

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
`Math.fround` reference.

**The indexes are features, and a build without one opens a file that
declares it.** `fenec-core`'s `vector`, `text`, `sparse` and `sorted` (the
four are `indexes`, on by default) are what a browser module may leave out:
`make wasm FEATURES="text sorted"`, `FEATURES=none` for none -- 144.6 KB
brotli with all four, 117.2 with none, and `make wasm-sizes` measures the
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
brotli; it is the size it was before the features, to the byte. A crate
that depends on `fenec-core` names `indexes` itself (the workspace takes it
without default features), and `fenec-ql` only as a dev dependency -- in its
normal ones it would put them back into every browser module.
`tests/features.rs` and the unit tests run without them in `make test`,
`web/fenec.test.js` hands files between the full module and the one
`make wasm-lite` makes, both ways, and CI runs clippy over none and each
alone.

**A quantized index holds codes, and `near` orders by the documents'
vectors.** `@hnsw(..., quant=int8)` keeps a byte a component over a scale a
vector, `quant=bit` the signs (cosine only). A code only estimates a distance,
so `Space` (`engine.rs`) takes the beam's `ef` candidates and puts them in
order by the vectors read out of the store, as `rerank` does; `exact` and a
filtered set searched exactly read the store as well, so every score is exact.
Bit codes need the wider beam `BIT_EF_SEARCH` -- 400: over a million
clustered 768-dim vectors a beam of 100 held 82.5% of the true ten, 400 held
98.4%, int8 codes 97.1% at 100 (`make quant-bench`). How well bits estimate
depends on the vectors: spread in every dimension, 36% at 100. The code kernels are scalar on
every target in `strip8!`'s order, so they need no SIMD twin to agree with the
browser. A graph over codes is record version 4; every other graph stays 3,
so no file is rebuilt for the feature.

**Filtered `near` needs its fallback.** The filter's rows are probed first -- in
blocks spread over the collection, and only until more than `ef × m0` match,
which is all the plan needs to know. A set that stays under that is searched
exactly; a larger one goes through the ANN with each candidate tested against the
filter. That second path *must* fall back to searching the whole set (the probe
carries on from where it stopped) when the result lands under the limit —
otherwise a filter correlated with the vector eliminates every candidate and
returns empty. The probe decides when rows are read, never the answer:
`tests/filtered.rs` checks it against the plan with the whole set found first.

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
derived data like the hash and text indexes: built on open, never in the file.

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
`make small`'s 1039. A comparison walks ICU's three levels, letters then
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

**`lookup` is one bucket probe per parent, not a join.** It attaches another
collection's matching documents to the row they belong to, and a `limit` after
it counts children *per parent* -- the shape a join cannot express. It is
terminal in the grammar: everything before it binds to the driving collection,
everything after to the looked-up one, which is exactly why there are no
qualified names (`reviews.stars`) in the language and no filter splitting in
the planner. The child key must be `id` or carry `@hash`; refusing an
unindexed one follows `near` and `match`, because a silent full scan of the
child collection would be a different feature under the same name. Nesting
lives in `ResultSet.nested`, never in `Value` -- JSON transports nest, the
PostgreSQL wire flattens (`ResultSet::flatten`), and the "no nested objects"
rule stands. Measured at 22.7 us in process against 0.332 ms for the same
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
builds an inverted index that is rebuilt from the documents on open — 27 µs per
document against the HNSW graph's ~102 µs, and the rebuild pass already reads
every document for the hash indexes. Nothing about it reaches the file, so
there is no validation path and no stale-index case to handle. It is shrunk to
fit where it is known complete (rebuild, `create index`); live ingest keeps
`Vec` growth slack.

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
had -- both searches, the vector index's `HashMap<DocId, u32>`, the text
index's `best_first` sort -- because in types of its own it was 11 KB of the
browser module; this way it is 2.

**`sparse<N>` is pgvector's `sparsevec`, and `@inverted` answers exactly.** A
sparse vector is held as its non-zero entries, `(index, weight)` ascending
with indices from 0, and travels everywhere in pgvector's text form,
`{1:0.5,3:0.25}/N` with indices from 1 -- a JSON string, pg text, a FenecQL
literal -- so a pgvector client and `fenec import` carry the same vector.
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
engine's one `(DocId, f32)` sort and maps dimensions through the vector
index's `HashMap<DocId, u32>` -- its own were 12 KB of the browser module; the
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
to visible: p50 0.32 ms (`make follow-bench`).

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
not build, answer empty -- the old behaviour -- so a tool never stalls. It is
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
names.

**`/_stats/statements` counts by shape, a tenant's apart.** Every
statement is also counted by its text with each literal and parameter as
`$n` and a list of literals alone as one (`fenec_http::statements::shape`)
-- over HTTP by the FenecQL of `/query` and `/batch`, noted as
`api::parse_query` reads it (`statements::text`), since the strings of the
JSON would all have been one shape. The counts are shards a thread each
behind a mutex only a reader of the whole also takes, 5 000 shapes at most,
the least called forgotten: 0.34 us for a statement of 87 bytes, against the
2.2 us its parse takes (`make statements-bench`). Over the pg wire they are
`pg_stat_statements` (a `fenec-catalog` table, filled only for a query that
names it). They need what reads the data without a JWT's scope, or the admin
token -- a shape names every collection -- and a tenant node keeps each
tenant's apart (`statements::View`): its own under `/t/<tenant>/` and over
its connection, every tenant's, named, to the admin alone.

**`integrations/` may use outside packages; the crates may not.** The
LangChain and LlamaIndex vector stores (`integrations/python`, one package,
the standard library for its client) and `useLiveQuery`
(`integrations/react`) are held to their frameworks' own tests --
`make python-test` runs LangChain's standard suite and the tests LlamaIndex's
integrations run from a `python:3.13` container against a fenec-pg started
here, `make react-test` runs the hook against a real replica, and CI runs
both (`integrations`), with the wheel and the npm tarball built as a publish
would build them -- publishing itself is not decided there. With
`full_text=True` a store indexes its text for BM25 as well and searches by
the words (`match`) or by the words and the vector fused (`fuse`):
LlamaIndex's `TEXT_SEARCH` and `HYBRID`, LangChain's `mode="text"` and
`"hybrid"`. `alpha` is not read, since `fuse` adds ranks; `quant` passes
`int8` or `bit` codes to `@hnsw`. A store names
its collection and metadata columns in the statement's text, so both are
checked against FenecQL's name pattern; values always go in as parameters,
and `in` takes one per element (`in [$2, $3]`), since a parameter binds a
value and not a list.

**Profiles differ on purpose.** `fenec-cli` uses the `cli` profile (`panic =
abort`, single process, nothing to recover). `fenec-pg` stays on `release`: a
panicking connection thread unwinds and drops only its own session.

## Conventions

- `Error` (`fenec-core/src/error.rs`) is the single error type: allocation-free
  variants, no `Box`. `fenec-pg` maps it onto PostgreSQL SQLSTATE codes.
- Unit tests live inline in `#[cfg(test)] mod tests`; cross-crate and protocol
  tests live in `crates/*/tests/`. Measurement programs are
  `crates/fenec-core/examples/` and are wired to `make` targets, not to CI.
- Comments explain *why* a thing is the way it is — a measured cost, a trap that
  was hit, an alternative that was rejected. Match that when adding code.
- All prose in the repo (comments, docs, README) is English.
