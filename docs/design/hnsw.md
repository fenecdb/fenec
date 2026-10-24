# The HNSW graph

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *The HNSW graph*: the graph as derived data, its records, its restore and open, its linking, and vectors written again. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## The graph is derived

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

## Restored arena

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

## Flat graph records

**A graph record is laid out flat.** Versions 7, an image's, and 8, a
server's tail -- 9 and 10, the aliases added, are what is written now
(below) -- hold the nodes' documents, flags, levels and level-0 lengths
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
Natively a graph of `LISTS_SHARED` (8 192) nodes or more has its level-0
lists decoded `LIST_SHARE` (2 048) nodes at a time on every core, once a
walk over the lists' widths has found where each share starts, and its
levels above beside them on a thread of their own (`lists_in_shares`), each
share held to end where the next starts: in turn they were 2.6 ms of an
8.6 ms open at 100 000 x 128, a stretch no other core worked in, and are 1.3
-- the walk 0.7 of it. Below 8 192 nodes the threads cost more than they share: a
graph of 1 000 opened in 0.197 ms that way against 0.166 in turn. Each width
of a document id is its own loop, and the flags are checked before they are
copied: a `memcpy` a document, the arrays were 0.7 ms, and are 0.3. The
browser decodes in turn as it did, its module 325 bytes smaller raw.

## Parallel linking

**A batch links in parallel, into the graph it would link in turn.**
`insert_batch` finds a batch's neighbours on every core against the graph
before it (`compute_candidates`); `link_batch` then sets each node's own
lists and groups what they add to their neighbours' lists by list, each
group applied in node order on whichever thread is free (`spread`). A
pruning reads only the list it prunes and the vectors, so every list ends
as linking the nodes one after another left it, and
`a_batch_links_as_its_nodes_would_one_after_another` holds the graph
records to that byte for byte over every arena. Linked on one core, the
pruning was 70% of a 100 000 x 128 build: 10.6 s then, 5.3 after (3.6-3.8
s since, with NEON's strips and the prefetch, below); 48.9 -> 23.4 s at
768 dimensions, and 50.6 -> 22.0 s for a crashed file's vectors linked at
the open. A share of the work a thread left the M1's efficiency
cores finishing last while the rest waited, so the work goes out an item
at a time. The browser has one thread and links in turn (`link_node`),
through the same pruning (`GraphView::pruned`).

## Upper layer beam

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

## Block linking

**A block's `put`s link their vectors together.** A block
`Database::begin` opened -- a `/batch`, a keyed write -- is natively its
statements' batch: a `put` in it leaves its vectors waiting
(`defer_batch`, `Block::waiting`), and they are linked on every core 512
at a time a field (`LINK_AT`, `link_waiting`), the rest as the block lands,
before its record is written, so that a block put back after all is put
back as any other. A search in the block measures the waiting ones
exactly, as it does a server's backlog, and a rollback drops those its
undo made tombstones (`forget_waiting`) -- the newest of the waiting, since
nothing else leaves one while a block is open. A lone statement links as
it did, and so does the browser module's `run` of several: its one thread
links a vector as it is written (`DEFERS`), the waiting being 843 bytes
brotli of it for nothing. Linked a row at a time, as a `/batch` of single puts would have
them, 100 000 128-dim rows went in at 5.3k rows/s with the graph kept;
linked together, at 16.0k, and 18.3k as a `/batch` of single puts over HTTP now (`make load-bench`).

## Deferred linking

**A server answers before its graph is linked.** A server checkpoints only
on its way down, so a crash after a long run leaves every vector written
since in the tail, and linking them at the open kept the port closed for as
long as they took: at 100 000 x 768 never checkpointed, `fenec-server` answered
its first `near` 67.7 s after it started, and linking at the open still
took 18.4 s with the lists pruned in parallel (14.9-16.3 s now, below). `fenec-server`, a tenant and a replica
open with `fs::open_serving` instead, and it answers after 1.23 s: those
vectors go into the arena unlinked (`VectorIndex::defer_batch`, every vector
of a graph the open cannot restore too), a search measures each of them
beside what its walk finds -- so an answer is never missing one -- and
`fenec_http::link::beside` links them on a thread of its own, slices of
about 10 ms under the write lock, each at most twice the last (a pace taken
over a small graph had a slice hold the lock for 112 ms). `near` takes the
exact scan's 10.5 ms until the 19.2 s of linking are done and 0.31 ms after,
recall 0.976 against 0.978 (`make reopen-bench`). A checkpoint meanwhile
writes the waiting nodes flagged -- a node's flag in the flat layout, where
the varint one, which only the tests write now, took graph record version 5
and only then -- and an open that does not
defer links them there. The linking needs the
lock to let a waiting writer in: Linux's std lock does, while on macOS
readers slip past it, and four clients asking back to back kept it from
finishing in eleven minutes -- as they would keep any write waiting. The
browser has none of it (`vector::UNLINKED`: 1.1 KB brotli).

## Graphs in the tail

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
waiting again. It is a graph record (kind 4) of version 6, 8 laid out flat
and 10 with the aliases (below), which an older binary does not know and rebuilds from -- it restored a record in the tail
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

## Aliases

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
the copies table's hash halves after those, below); a restore
checks each alias's document holds the node's vector. Tests whose
data repeated vectors (`i % 13`) to count nodes and tombstones now write a
vector a row. The browser module 2.2 KB brotli; builds, searches and
opens as fast.

## The copies table

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
The browser module kept its table and its hash (`cfg`): 618 010 bytes as
on main, every function the size it was, until it took the native hash
with the halves (below).

## Halves

**A graph record carries the halves the copies table is made from**
(`HALVES`, `Same::halves`). The first put after an open read and hashed
every vector to make the table: 33-169 ms over 1.1 million of 128
dimensions. Hashed as the restore fills the arena, each vector in cache,
the table would cost no put, but the open of a checkpointed 100 000 x 128
took 0.45 ms longer (3.8%) and of 100 000 x 768 2 ms (6.7%), and four
vectors' chains side by side hashed only 1.65 times as fast. So a version
9 or 10 record ends with a `HALVES` byte and the top half of each node's
hash, 4 bytes a node, a tombstone's 0: 0.68% of a 128-dim file, 0.13% of
a 768-dim one. The restore keeps them -- once the nodes at 64 places
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
table's slots (`halves_by_node`, on every core past `SAME_SPLIT` nodes:
8.3 -> 2.3 ms at 1.1 million) -- natively, a split table's on the threads
that write the record's lists, a share of the shards to each besides its
part (`put_all_lists`): read after the lists, they held a server's read
lock as it keeps a graph of 110 000 nodes 6.07-6.34 ms p50 against
5.85-5.88 without them, and beside them 5.83-5.98 (100 saves a run, five
runs each in turns) -- or hashes the arena where neither holds them, so a
record is the same whatever way its index came to be. The record keeps
its version: the reader before stops at the aliases, so a binary from
before reads it without them -- this build refuses anything else there --
and a record without them, a binary's from before, has the table made
from the arena.

## Halves in the browser

**The browser module hashes as a native build does, and takes the
halves.** Its `Same` hashed with one chain of multiplies (`hash_words`),
so it could use no server's halves, and its first put after a load hashed
every vector: 13.1-14.6 ms at 20 000 x 384, 21-34 at 100 000 x 128, the
puts after it 0.3-0.5. It hashes with `hash_lanes` now, a slot placed by
its half as natively, and reads and writes the halves as a native build
does; `web/fenec.test.js` finds the bytes
`the_halves_a_record_carries_are_the_browsers` holds a native record to in
the module's image of the same vectors. Its first put after a load went
13.1-14.6 -> 1.3-2.1 ms at 20 000 x 384 from its own image and 12.7-13.1
-> 1.2-1.9 from a native file, 21.6-24.1 -> 2.8-4.2 and 21.3-30.7 ->
3.0-5.9 at 100 000 x 128; its loads (65.4 against 65.6 ms median, 25.8
against 26.0), builds, puts, `near`, filters and JSON (`make wasm-speed`)
as they were, and the graph and its aliases the same bytes. 1 221 bytes,
543 gzip, 197 brotli (`make size-report`): the module holds one copy of
the hash, over the bytes the arena would hold -- an f32 vector written
out as a code is -- where another over the floats was 0.4 KB more;
checking the halves in a plain loop rather than through `filter`,
`step_by` and `all` took 0.5 KB off, and appending them a half at a time
rather than into room made first 0.4 KB.

## Opening a graph

**An open of a file with a graph copies its vectors, and that is most of
it.** `make compare`'s reopen once read 9 ms against SQLite's 0.9: one open
and an ANN query of fenecdb's against SQLite's open and a count through its
index, two different first questions. It now times each side's open alone,
then with a read by id, then with a vector top-10 -- SQLite's a full scan --
the median of 31 each: 6.0, 6.0 and 6.1 ms against 0.07, 0.46 and 30.5.
SQLite reads nothing until it is asked. Of fenecdb's 6.2 ms at 100 000 x 128
(`examples/opens.rs`, timers in each part), 3.6 are the arena -- 51 MB of
vectors read out of the mapped file, made unit ones and written into pages
the open allocates, on every core -- which the pages bound: each page of the
62 MB file touched once took 1.5 ms on eight threads, each of 62 MB newly
allocated 4.8 (2.7 on one), and with the arena filled on one core the open
took 17.7 ms against 10.5. Then 1.8 the graph record (`Read`), 0.3 `by_doc`
and 0.3 the id index. Before, 8.6: the graph record 3.5, the id index 1.0,
every collection's ids listed for nothing 0.2 (all above). A million rows of
128 dimensions open in 60 ms against 86, a million of no vector in 1.3
against 10.3, a thousand of 128 in 0.156 against 0.165, and the browser
module loads as fast as it did. Rejected: the arena's pages written once each
on a thread beside the record's read, as `MAP_POPULATE` would -- 7.63 ms
against 7.50, the two's faults contending; a restore left to the first
statement that needs the graph, as the derived indexes are -- an open would
no longer validate the graph it holds, and the first `near` or write would
pay the 6 ms; the arena kept in the file for an open to map rather than copy
-- every 100 000 x 128 file 51 MB larger, its vectors twice.
