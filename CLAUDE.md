# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

fenecdb — a minimal, vector-native embedded database in Rust. Compiles to WASM for
the browser, has its own query language (FenecQL) and speaks the PostgreSQL v3 wire
protocol. The docs under `site/content/docs/` are the long-form reference (design
rationale, benchmarks, full FenecQL and HTTP surface); `README.md` is the
front door and links into them, and this file is the working summary.

## Commands

```bash
make test          # cargo test, then the JS tests (node --test)
make wasm          # builds fenec-wasm for wasm32, copies to web/fenec.wasm
make serve         # wasm + python3 http.server -> http://localhost:8787
make bench         # scale measurement (fenec-core/examples/bench.rs)
make memory        # memory footprint, for calibrating --max-memory
make sweep         # ef / recall trade-off
make compare       # vs SQLite + pgvector (needs `make pgvector-up` first)
make python-test   # LangChain + LlamaIndex stores vs their frameworks' tests (Docker)
make react-test    # useLiveQuery vs a real fenec-pg replica (needs `make wasm`)
make beir BEIR=dir # nDCG@10 per ranking path (vectors: crates/fenec-bench/beir, embed.mjs + splade.mjs)
make import-test   # the PostgreSQL arm of import and --follow (needs Docker)
make follow-bench  # --follow: commit-to-visible latency, drain, reconnect (pgvector-up first)
make small         # smallest `fenec` binary: --profile cli --no-default-features
make pg PGPASS=secret HTTP=127.0.0.1:8080   # run the server against ./data.fenec
make node ADMIN=secret   # a tenant node: fenec-pg --dir tenants (PG=addr adds the pg wire)
make shard               # the router in front of the nodes (./shard.fenec)
make shard-bench         # router overhead per request, tenant move time
make replica-bench       # replica lag per sync policy, catch-up, what a failover loses
make maintenance-bench   # reads and writes during create index / compact
make open-bench          # opening a 1 GB file, read into memory or mapped
make reopen-bench        # a crashed 100k x 768 file: linked at the open, or beside the queries
make quant-bench         # quant=int8|bit against full vectors: memory, recall, latency
```

Single tests:

```bash
cargo test -p fenec-core --test persist          # one integration test file
cargo test -p fenec-ql near                      # by name substring (integration: fn name only)
cargo test -p fenec-core codec::tests            # inline unit tests in a module
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
fenec-http  (REST/JSON + SSE, tenant registry, replication, /_metrics)
     |                     \
fenec-pg    (wire protocol:  fenec-shard (tenant router: directory,
     |      server AND client,            placement, move)
     |      catalog from fenec-catalog)
fenec-import (SQLite file reader + PG COPY source + --follow)
     |
fenec-cli   (`fenec` shell, `fenec import`, `fenec types`)
```

`fenec-bench` is a measurement harness (`publish = false`) and is the only crate
allowed external crates — that is where `rusqlite`/`postgres` live.

`fenec-core` modules: `store` (segments, offset index), `engine` (`Database`,
`Collection`, replay/snapshot/compact/checkpoint), `vector` (HNSW + distance
kernels), `text` (tokenizer, inverted index, BM25), `query` (`Statement`, plan
execution), `schema`, `value`, `codec`, `collate` (ICU's Turkish order, a
generated table),
`json`, `num` (decimal text to `f64`), `time` (calendar arithmetic), `sparse`
(sparse vectors and their inverted index), `changes`
(the change ring), `plugin` (registry), `fs` (buffered file I/O, behind the
`std-fs` feature).

The browser client is `web/fenec.js` — WASM glue (~190 lines), the query builder,
the HTTP client and the sync layer, in one dependency-free ES module. `web/fenec.d.ts`
holds the types; `fenec types <file>` generates schema-specific declarations.
`persist`/`restore` keep a database in IndexedDB as a file would hold it: an
image, then the writes since as chunks, from the journal `fenec_journal` starts
and `fenec_drain` empties (off until asked for -- a page that never drains would
hold every write). One row persists in 0.6 ms over 32 MB, against 136 ms for the
image.

## Invariants worth knowing before you change things

**Zero dependencies is a hard rule** for `fenec-core`, `fenec-ql`, `fenec-wasm`,
`fenec-http`, `fenec-pg`, `fenec-import`, `fenec-shard`, `fenec-catalog`. The WASM output has to stay small and
auditable; own codec, own JSON, own HNSW, own SCRAM/crypto, own decimal-to-`f64`
(`str::parse` drags in a 12 KB table -- see `num.rs`). `fenec-core` does
dev-depend on `fenec-ql` (Cargo allows the cycle through a dev dependency) so tests
can write real queries.

**No page cache, and the file is mapped.** The byte sequence on disk and in
memory are the same format, so a read decodes straight over the bytes --
there is no eviction policy, no dirty pages and no cache of fenecdb's own.
`fs::open` maps the file where the target maps files (unix, 64-bit), so the
documents stay in it and the process holds what it derived from them: the
offset index, the hash, ordered and text indexes, the graph, and the writes
since the open. A 1 GB file of 2.3 million rows with a hash and an ordered
index holds 188 MB that way against 1 095 read into memory, and its
`compact` peaks at 236 MB against 2 012; a 10 GB file opens on an 8 GB
machine, which read it cannot. `fs::open_in_memory` (`fenec-pg --no-mmap`,
which reaches a replicated file and a `--dir` node's tenants as well) is the
other way, for a network file system or to have `--max-memory` cover the
data. A new file is mapped from the start. A rewrite -- `checkpoint`,
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

**Single writer.** Reads take a shared lock (`Database::query`), writes the
exclusive one (`execute_with`). There are no transactions — `fenec-pg` accepts
`BEGIN`/`COMMIT` and does nothing with them, and refuses (`0A000`) a `ROLLBACK`
that follows a write in the block rather than answering "done". Two processes
opening the same file corrupts it, which is why `fenec-http` is a second
listener inside `fenec-pg`, never its own binary.

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
history (record kind 8, `History`) is the one record that moves none, and it is
never sent: a promotion forks it, a replica is continued only from a position
the primary's history passed through and sent an image otherwise, and a
following database refuses writes (`Error::ReadOnly`, `25006`). Lag is 0.20 ms
p50 under `--sync always` and at most 283 ms under `--sync 250`; ten failovers
under `always` lost no acknowledged write. An archive (`fenec archive`,
`fenec-http/src/archive.rs`) is the same stream written to files, each write
with the time the primary appended it; `fenec restore` is an image plus the
archived writes up to a time or a change, forked -- a fenecdb file is exactly
that, so a restore is a concatenation checked by opening it.

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
the next instance. The router never promotes on its own: it cannot tell a node
that is gone from one it cannot reach, and guessing makes two primaries. A
write is on the standby 0.089 ms after the primary answered it (p99 0.448),
and 20 tenants failed over in 60 ms (`make shard-bench`).

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
mapped file a compact copies no record: the graphs holding tombstones are
rebuilt beside the database, and the live records streamed into the new file
under the write lock. Only a lone statement takes this path; a batch, the
shell and `execute` hold the lock.

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
the id counter (kind 7) exists so `compact` cannot hand out a deleted id again;
the history (kind 8) is the one appended record that is not a write.

**The HNSW graph is derived data, not a cache.** It is written only by
`snapshot`, `compact` and `checkpoint` — never on the write path. On open the
version, dimension, precision and link bounds are validated, and the live nodes
against the documents holding a vector; anything off means a silent full
rebuild. A corrupt graph can therefore never lose data. It is restored where the
checkpoint's image ends, against the documents it was written with, and the tail
after it is applied as the write path would (a touched document keeps its node
while it holds the same vector, and has it retired for the new one otherwise) --
restored after the whole file, one write in the
tail threw it away, and a crash cost 48 s at 100 000 x 768 instead of 0.99. A
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

**A server answers before its graph is linked.** A server checkpoints only
on its way down, so a crash after a long run leaves every vector written
since in the tail, and linking them at the open kept the port closed for as
long as they took: at 100 000 x 768 never checkpointed, `fenec-pg` answered
its first `near` 67.7 s after it started. `fenec-pg`, a tenant and a replica
open with `fs::open_serving` instead, and it answers after 1.23 s: those
vectors go into the arena unlinked (`VectorIndex::defer_batch`, every vector
of a graph the open cannot restore too), a search measures each of them
beside what its walk finds -- so an answer is never missing one -- and
`fenec_http::link::beside` links them on a thread of its own, slices of
about 10 ms under the write lock, each at most twice the last (a pace taken
over a small graph had a slice hold the lock for 112 ms). `near` takes the
exact scan's 16 ms until the 61.8 s of linking are done and 0.46 ms after,
recall 0.976 against 0.978 (`make reopen-bench`). A checkpoint meanwhile
writes the waiting nodes flagged, in graph record version 5 and only then,
and an open that does not defer links them there. The linking needs the
lock to let a waiting writer in: Linux's std lock does, while on macOS
readers slip past it, and four clients asking back to back kept it from
finishing in eleven minutes -- as they would keep any write waiting. The
browser has none of it (`vector::UNLINKED`: 1.1 KB brotli).

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
`Math.fround` reference.

**A quantized index holds codes, and `near` orders by the documents'
vectors.** `@hnsw(..., quant=int8)` keeps a byte a component over a scale a
vector, `quant=bit` the signs (cosine only). A code only estimates a distance,
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
vector. Bit codes need the wider beam `BIT_EF_SEARCH` -- 400: over a million
clustered 768-dim vectors a beam of 100 held 82.5% of the true ten, 400 held
98.4%, int8 codes 97.1% at 100 (`make quant-bench`). How well bits estimate
depends on the vectors: spread in every dimension, 36% at 100. The code
kernels add in `strip8!`'s order on every target, so a graph over codes is the
browser's graph bit for bit; on aarch64 the int8 strips are NEON intrinsics
(`vector::neon`), because the vectoriser widened codes through a register it
also accumulated in, which chained every strip to the one before -- 5x slower,
or not, depending on the code around it. A graph over codes is record version
4; every other graph stays 3, so no file is rebuilt for the feature.

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

**`collate tr` is ICU's order, and `order`'s alone.** Its weights are ICU's
own -- `tools/collate/gen.py` reads them out of macOS's libicucore into
`collate/table.rs`, one `u32` per code point over the Latin script, the
combining marks and general punctuation -- and a comparison walks ICU's three
levels, letters then accents then case, over the whole string before it falls
back to the bytes, so the order is total. `web/fenec.test.js` holds it to
`Intl.Collator("tr")` on every run; what it does not do is ICU's
normalisation, so two marks on one letter out of canonical order can sort
apart. A comparison starts at the first byte the two strings do not share, a
character earlier when that is a mark, since `c` and U+0327 are one letter:
68 -> 30 ns a comparison over a million names. A `@sorted` field keeps byte
order and is never walked for a collated key, and `where` compares bytes --
collating a comparison would need an index that orders the same way. It
costs the browser module 8.5 KB, 3.2 KB brotli.

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
would hide every capitalised Turkish word.

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

**`sparse<N>` is pgvector's `sparsevec`, and `@inverted` answers exactly.**
A sparse vector is held as its non-zero entries, `(index, weight)` ascending
with indices from 0, and travels everywhere in pgvector's text form,
`{1:0.5,3:0.25}/N` with indices from 1 -- a JSON string, pg text, a FenecQL
literal -- so a pgvector client and `fenec import` carry the same vector.
Every way in goes through `sparse::normalise` (order, an index given twice
refused, zeros dropped), and the index relies on it. `@inverted` is the text
index's shape with weights where the counts were; `near` by dot product
walks it with MaxScore, rank-safe -- the bounds are compared once rounded to
`f32`, after a 1e-9 slack, so a tie-break never depends on the pruning --
and `tests/sparse.rs` holds it to `near ... exact` row for row. Only a
document sharing a dimension with the query is ranked. Like the text index
it is derived and never persisted. It sorts through the engine's one
`(DocId, f32)` sort and maps dimensions through the vector index's
`HashMap<DocId, u32>` -- its own were 12 KB of the browser module; the
feature costs 16.2 KB, 4.3 KB brotli. SPLADE++ (`beir/splade.mjs`) scores
nDCG@10 0.693 on SciFact against `match`'s 0.662, and 0.331 on FiQA against
0.232 (the dense vectors 0.366), at 2.6 ms p50 over 57 638 documents: its
queries' 37 to 65 terms reach most of the corpus. Both BEIR scripts cut
texts themselves: transformers.js drops the closing [SEP] when it truncates,
SPLADE without it took SciFact to 0.23, and the dense vectors (`embed.mjs`)
moved by up to 0.003.

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

**`integrations/` may use outside packages; the crates may not.** The
LangChain and LlamaIndex vector stores (`integrations/python`, one package,
the standard library for its client) and `useLiveQuery`
(`integrations/react`) are held to their frameworks' own tests --
`make python-test` runs LangChain's standard suite and the tests LlamaIndex's
integrations run from a `python:3.13` container against a fenec-pg started
here, `make react-test` runs the hook against a real replica. A store names
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
