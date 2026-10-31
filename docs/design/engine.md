# The engine's lock and blocks

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *The engine's lock and blocks*: the single writer, a block's documents built beside the readers, long reads pinned, and a block as one record. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## Single writer

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
2.0M beside blocks of 1 000 writes landing back to back, p99 2.5 us and the
longest 0.5-1.2 ms, where SQLite's WAL reads the last commit meanwhile,
630k/s, p99 59 us and 0.4-1.9 ms at most -- a block's documents built beside
the readers and its buffer written behind the lock (both below); built under
it, 76-292k/s, p99 0.52-1.48 ms and 5.5-6.2 at most in the same session (213k
and 4.8 quoted before), and before the handover counted writes the longest was
17.4, a handover of 930 000 small documents. Durable writes gain from the fsync outside the
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

**What takes `<file>.lock`.** A process that opens a file to write it, or
renames a database file into its place, takes `<file>.lock` first
(`fenec_abi::lock::hold`: a record lock, the process's, and a list of this
process's lock files, [native-and-sync.md](native-and-sync.md#the-native-library)),
and is refused while another holds it: `fenec-server --file`, the native
library, `fenec mcp --file`, and the shell -- `fenec <file>` typed, `-c`,
piped or a script, a read too since the lock is taken at the open --
`fenec import` into a file (one-shot and `--follow`), `fenec backup` into a
file and `fenec restore` into one (a rename over the file, its sync log
removed first, which a server with it open would have gone on writing past
and lost), and `.save <path>`, which writes beside the file and renames
over it, as a mapped file is replaced, and is refused over the session's
own file. The shell took none up to 0.1.12: beside a `fenec-server --file
x.fenec`, `fenec x.fenec -c 'put ...'` printed `1 records` and exited 0;
the server, which reads its file only at its open, never saw the row, and
its next writes were appended at its own end of the file, over the
shell's. A shell write of 300 bytes followed by two short server writes
left the tail of the shell's body behind them, and the next open, the
shell's or a server's, refused the file (`corrupt: unknown record kind
120`, the `x`s read as a kind): one write lost, then the file. Since the
holder writes its id and program into the lock file as it takes the lock,
a refusal names it -- `x.fenec is open in another process (fenec-server,
pid 4242)` -- from the system's `F_GETLK` and that line, the line taken
only where its id is the one the system names (under a holder of an older
release, which writes none, the line an earlier holder left would name the
wrong program), and the file is opened to ask only once it is known not to
be this process's own, since closing a descriptor of it lets this
process's lock go. The lock, the naming, `--read-only` and `.save`'s
rename are 10.2 KB of code in the smallest `fenec` (`make small`, 16.7 KB
of file on arm64 macOS, whose pages are 16 KB). A tool that only looks takes none and
writes nothing: `fenec --read-only`, `fenec mcp --read-only` and `fenec
types` open with `fs::open_read_only`, which maps the file at its length
at the open, passes a record a server is in the middle of appending over
as a torn tail, cuts nothing and plays no sync log -- the file only grows
and is replaced only by rename, so the bytes it read are never written
under it; it reads the file as it stood at the open. The shell refuses a
write there before the engine sees it: handed over, the read-only sink's
refusal was a storage error, after which every write said so.

Not covered yet: a `--dir` node takes no lock on its directory or on a
tenant's file, nor does `fenec-shard` on its directory file, so a shell
over a tenant's file, or two routers over one directory, are not refused.
The lock file is named after the path as given, so a file opened through a
symbolic link to it is locked under another name. Elsewhere than on a Unix
`hold` takes no lock at all (no release is built
for Windows); `LockFileEx` over a range past the holder's line is what it
would take there, since a Windows lock is mandatory and would refuse the
reads of the line.

## Prepared blocks

**A block's documents are built beside the readers** (`engine/prepared.rs`,
`Database::prepare`). A block holds the write lock from its first write to
its record, and a block of 1 000 single puts held it 0.45 ms p50 on this
Mac, 1.3 at p99, every reader waiting. Sampled (`sample`, 1 kHz), the time
under the lock was the schema copied whole for each statement (17%), each
document built from its pairs and parameters and each value made its field's
type (20%), its payload encoded with every value copied first (12%), the
document freed once written (10%), the collection looked up by name five
times a statement (8%), each frame made in a `Vec` of its own and copied
twice (6%), the record and the handovers (6%). Natively the schema is an
`Arc` (`SchemaCell`, a plain value in the browser, its module 80 bytes
smaller raw and 95 larger brotli, the layout's noise), each value encoded
where it lies (`encode_doc_into`), the frame written once into the block's
frames and copied from there into the segment (`Store::append_onto`), the
hooks borrowed and the collection looked up once -- which took a lone put in
memory from 599-618 to 412-431 ns, 687-691 to 482 under `@hash`, a `del`
705-714 to 617-626 (`make ttl-bench`'s `writes`, in turns), and a writer
with no fsync from 1.47-1.49M to 1.78-1.91M a second. Then `prepare`, under the
read lock, builds and encodes the documents of every `put` among a block's
statements into one buffer for the block -- buffers of its own a statement
were three allocations each, a quarter of what they saved -- and the block
takes them (`execute_prepared`; `execute_ready` for a `/batch` run a
statement at a time, `fenec_http::held::prepared`) where its collection
holds the very schema they were built against: the preparation holds the
`Arc`, so any change copies it and `Arc::ptr_eq` fails, and the `put` builds
its own as it always did. Everything the database decides -- the id, an
insert over a held id, `@unique`, `if absent`, the hooks, the indexes, the
record -- is decided under the lock as before, so the file is the same byte
for byte (`tests/prepared.rs`, against twins built under the lock). A hook
that may change a document (`Hook::rewrites`, true unless it says not;
`Check` and the mirror's `Guard` judge only) has its payload encoded after
it. The documents written are freed after the lock, with the preparation. A
preparation a panic left half way is let go of as the next statement starts
(`disarm`): taken, a block that came after it wrote another's documents. A
server prepares a `/batch` or a `/query` whose `put`s write `PREPARE_AT`
(16) documents or more: fewer spend more on the read lock and the buffers
than they take off. The lock is held 0.14-0.15 ms p50 for the block,
0.38-0.39 at p99 and 0.44-0.50 at most over 10 000 blocks, against
0.31-0.35, 0.59-0.73 and 4.6-50 built under it with the rest of the change; a writer
alone lands 1.6% fewer blocks prepared than built under the lock (the read
lock, the buffers, the documents cold again), and a fifth more than on
main. A REST insert prepares too (`early_put`): its body is read with no
lock held, against its collection's schema `Arc` taken under the read
lock, its documents built under a second where that `Arc` is still the
collection's, and the write lock takes both where the collection holds
that very schema (`Early::holds`), routing the body again under it
otherwise -- a field added, the collection made again, a vector field
made json are read and built again (fenec-http's
`a_rest_insert_whose_schema_moved_before_the_lock_is_read_again`, which
fails with the check taken out). Under the write lock a body of 1 000
rows was read, checked and built: a reader by id beside such inserts back
to back waited out holds of 0.40-0.47 ms p50 and 0.62-0.78 at p99 for rows
of a text and an int, 0.07-0.08 and 0.30-0.34 now, and of 3.25-3.73 and
4.35-5.49 ms for rows with a 128-dim vector, 0.18-0.21 and 1.26-1.74 now,
the writer as fast (`make concurrency-bench ARGS=rest`, in turns), and
`make load-bench`'s REST arrays went in at 212-221k rows a second against
216-217k, for 5.3 KB of `fenec-server`'s code. A body under `EARLY_AT`
(1 KB) with fewer `{` than 16 is read under the lock as before: read
first, a read lock, the schema's count and a look-up more took a row 1.11
-> 1.14-1.16 us in process. A token whose
rules read another collection is bound under the write lock, so its
insert routes there. The native library prepares a text's `put`s as well
(`fenec_abi::execute_ready`): the lists a json field keeps are read first
(`exact_noting`, which notes whether it read anything anew -- returned
beside the error, the `bool` grew the browser module's `fenec_query` 48
bytes), again under the write lock only where a write landed meanwhile,
and the preparation is let go of where that read anything anew. An app's
read by id beside texts of 1 000 128-dim rows waited 0.24-0.40 ms p50
against 0.81-1.07, read 209-275k times a second against 108-143k (`make
ffi-bench`'s `beside`), the p99 3.2-7.0 ms either way: the native library
wrote its buffer under the lock as it filled, and writes it behind the lock
now (below). A lone buffered put is what
it was (1.96 us p50 against 1.94, the mean 2.25 either way, `ffi_bench put`
in turns), and the library's code 6.5 KB more. The shell has nothing
reading beside it and builds under its lock. Rejected: a pin
taken as each block starts, for the readers to read the database as it
stood -- it copies the id index, 12 bytes a row, 0.25-0.37 ms over a
million rows, as long as the block and growing with the collection, and a
read an index answers would still wait; a copy-on-write id index in chunks
to make that pin cheap, a pointer more on every read by id; the whole block
run beside the readers, applied under the lock as a list of effects -- a
read in a block sees the block's writes, a `set` reads the row it writes,
`require` and `@unique` count under the single writer, so every read path
would have consulted an overlay of the block's writes; readers handed the
block's undo log to read the rows as they were, which reads by id alone
could use.

## Pinned reads

**A long read is pinned, and read with no lock held** (`engine/pinned.rs`,
`Database::pin`). Under the read lock a write waited for every read, so a
long one held them all: a ledger's reconciliation (a read-only `/batch`
of three aggregates over a million entries, 140-200 ms) held transfers up
to 187-512 ms, 36 past 50 ms in ten seconds (`make recon-bench`), and an
aggregate of 50 000 groups over a million events four writers' puts --
and every read by id behind a waiting one -- for its 0.8 s (`make
concurrency-bench ARGS=long`). `pin`, under the read lock, takes what a read
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

## Blocks

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
a binary from before 0.1.5, which refuses it as corrupt. Undone in one pass
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
anew they took a lone `put` from 832 to 985 ns; kept, a put cost 841
against the 829 before blocks, a `del` 648 against 634.
