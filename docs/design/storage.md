# Storage

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *Storage*: the mapped file, the handover of writes to it, a block's spills, compaction and the file format. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## The mapped file

**No buffer pool of its own, and the file is mapped.** The byte sequence on
disk and in memory are the same format, so a read decodes straight over the
bytes -- there is no eviction policy, no dirty pages and no cache of fenecdb's
own. "Page cache" is the operating system's, which keeps the mapped file's
pages and starts cold after a restart, as for any file; it was called "no page
cache" here before, which read as nothing being cached.
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

## Handover

**A mapped database hands what it writes over to its file.** A block's
record goes into the file as it lands, and the stores held its documents in
their segments besides, until a restart or a compact: 250 000 768-dim rows
loaded into a server, the engine counted 1 592 MB against 818 for the same
file started again. Once the documents since the last handover amount to
`HANDOVER_AT` (16 MB), 65 536 records or 16 384 writes
(`HANDOVER_DOCS`: its pause is a document's work each, about 17 ns, and 16
MB of rows of a text and an int were 930 000 of them, the write lock held
16 to 17.5 ms; at 65 536 writes 0.82-0.89 ms, and at 16 384 0.26-0.31, a
block's p99 under the lock 1.04 -> 0.45 ms and its mean as it was), `Database::hand_over` has the sink
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

## Spills

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

## Maintenance beside the database

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
an `Arc`, in the browser module too now), the index copied, each graph's record written -- the live
records written into `<file>.beside` with no lock held and fsynced, and
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

## Compaction

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
started at 19k and took 25 s to 400k). Over nine runs each in turns, the
median 100 ms of a compact went 27.1k -> 30.6k, its worst 1.1k -> 3.6k, a
compact 10.9 -> 7.8 s, the reads during it p99 2.3-6.5 -> 1.3-5.3 ms; at
250 000 records, whose file fits, 143k -> 153k, the worst 16.9k -> 129k
against 180k with none, a compact 1.0 -> 0.3 s, the reads p99 0.5-1.3 ->
0.14-0.35 ms and 52 -> 18 ms at most. Paced to half and to a quarter of its
speed a compact left the writer where it was (30-35k) and took 56 and 95 s
against 10; at utility QoS its reads were throttled behind the writer's and
it took 138 s; with its side file uncached (`F_NOCACHE`) in a compact that
began once the file had outgrown the cache (below), or at utility QoS for
the CPU alone, nothing moved. A database
read into memory compacts beside itself too, the live records copied into
fresh segments with no lock held; it used to copy them under the read lock
and build every index again, though the indexes are keyed by id.

## Compaction and the page cache

**A file is compacted before it outgrows the page cache** (`cache.rs`,
`CompactPolicy::memory`, `due_in`). There is no cache of fenecdb's own, so
what the reads need in memory is the file's pages holding a live record --
and under updates at uniform keys that is every page until a compact: a
million 1 KB records updated until the file stood at 2.1 GB left one on
every 16 KB page and 94% of 4 KB ones (a simulation of the bench's keys).
Past the cache every read and update waits for the disk in turn, whatever
is dead: in a container held to 2 GB (`docker run --memory 2g`), at half
the file dead, the file stood over 2 GB 69-74% of the time and 5M updates
took 358-511 s, 3.5-4.1k a second at the median 100 ms against 79k while
it fitted, 0.81-0.90M major faults, compacts of 38-99 s; on this Mac beside
1 GB wired by a ballast (`mlock`), 82-535 s. So a file is compacted too
once a third of what the compact writes is dead -- four bytes written a
byte updated at most, against two -- where its live records fit the room
the page cache has for it (`cache::room`) and either the file has reached
three quarters of that room (`ROOM_SHARE`) or the process takes 500 major
faults a second (`cache::faulting`, `getrusage`, over a second or more).
The room is exact on Linux -- `MemAvailable`, and a cgroup's limit less
what the group holds that is not file pages, from its own group up -- and
not on macOS, whose figure (physical memory less the apps', wired and
compressed pages) grows under the pressure as it compresses the apps:
2.4-2.9 GB while a 1.5 GB file was read from the disk at 5-7k faults a
second. The faults are the macOS half: beside the ballast, the rule
without them took 226-361 s, with them 191-260. A compact's own
faults -- the old file's records read, the new file read in -- are
forgotten once it is done (`cache::forget_faults`): counted, the next
look found the process faulting, and a 1.6 GB file that took none was
compacted every 6 s. A compact whose old and new files would not fit
together, or that starts while the process faults, writes the new one past
the cache (`SideFile::past_cache`: `F_NOCACHE`, or synced and
`POSIX_FADV_DONTNEED` every 64 MB, since written pages stay until they are
on disk) and reads it in after the swap rather than before: through the
cache its pages pushed out the old file's, which the reads need until the
swap, and the writer ran at 5-6k updates a second through a compact, 8k
with the pages dropped once the image was written, 18-25k dropped as it
went; read in before the swap, the writer ran at 6.2k a second through
a compact and the swap held the lock 24-60 ms, after it 18.6k and 6-14 ms.
With all of it the container's 5M updates took 80-100 s against 358-511,
the file at most 1.9 GB, 34-39k major faults against 0.81-0.90M, compacts
of 3-6 s, the reads during them p99 0.44-0.49 ms against 4.6-6.5 (max
113-517 ms against 220-534: a read meeting the new file as it comes in
after the swap), for 3.0-3.2 bytes written a byte updated against 1.9-2.0;
on the Mac, 191-260 s against 82-535 and 3.3-3.6 bytes written a byte --
macOS gives a growing file more of the machine as it compresses the apps,
which main's 82 s run had and its 535 s one did not, and a file kept small
is not given it: steadier, never the best case. A file that fits moves
nothing: 250 000 records (29-31 s, the median 100 ms 174-181k, through a
compact 153-156k), YCSB A, B and F in process (with the memory free A at
16 threads 82-92k -> 133k, the rest within the runs' spread; beside a 1
GB ballast on a quiet machine, against the same build with `memory` off,
two ABBA quartets, A at 1 thread 50-99k -> 101-148k and at 16 47-60k ->
72-127k, B at 16 197-460k -> 475-576k, F at 1 78-90k -> 111-202k and at
16 71-83k -> 102-115k, B at 1 313-476k against 412-477k -- one pair
beside other agents' builds had A at 1 at 29.7k -> 24.1k),
`make concurrency-bench` and `make requests-bench` in ABBA turns as on
main, the browser module byte for byte (`cache.rs` is `std-fs`'s). Rejected: `MADV_RANDOM` (Linux reads 128 KB
around a fault, the neighbours the next reads want: 806-811 s, 561-608
with the rule), a look every second rather than 5 s (13 compacts for 10,
3.8 bytes written a byte, no faster), writing an update over its old
record (its torn write is no torn tail to cut but a corrupt record mid-file,
and a pin, a handover's `Landed`, the sync log, a replica's apply and the
archive all take the file as only growing), dropping the pages only dead
records hold (none, by the count above), a compact ordering the records by
heat (a uniform workload has none; YCSB's hottest quarter of 16 KB pages
takes 77% of reads in id order against 96% by heat, which matters only
once the live records outgrow memory, and E's scans want id order).

## File format

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
alone. Behind its documents the index says where each 16 384 of them end --
the byte, the id and the place the next share counts from, 24 bytes each,
and their number (`FRAMES_SPLIT`) -- and an open reads the shares on every
core, each into its own stretch of the dense id array (`adopt_shares`). Read
in turn through `get_uvarint` and `insert`, the index was 9.5 ms of a
million rows' open; a byte read inline and the next id pushed onto the dense
array (`push_above`), 4.8; in shares, a million rows of no vector open in
1.3 ms against 10.3 (`make opens-bench`). Ids that would leave the array a
quarter empty or reach the sparse map are read in turn, as `insert` places
them, and so is an index whose shares do not read as it says. A binary that
knows only `FRAMES_MARK` walks the record of a `FRAMES_SPLIT` image, as it
walks one with no index. Read into memory, a load copies each frame as it stands into the
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
