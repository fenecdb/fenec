# Replication and archives

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *Replication and archives*: replication, a node's tenants archived over one stream, `/_changes`, `Fenec-After`, sealed archives and `--follow`. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## Replication

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
following database refuses writes (`Error::ReadOnly`, 403). A promotion
halts the follower before it forks, and a follower reconnecting to a
primary that is gone walks away from the dial at the halt
(`fenec_http::dial::connect_unless`: the lookup and the connect on a
thread of their own, the stop flag looked at every 10 ms, the stream
handed to `halt` before its request goes out): waited for, the halt took
the connect's 5 s where the dead machine's address swallowed packets, and
as long as the resolver took to refuse its name -- the tail of a failover
in V5's drill on CI; `tests/replication.rs` holds a promotion beside a
lookup that never ends (30 s on main, the 5 s of the dial's bound with the
dial not walked away from) and beside a primary that takes the connection
and never answers (the follower's 10 s of silence where the stream reached
`halt` only with the head). A durable
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

## A node's archive

**A node's tenants are archived over one stream** (`fenec archive
--tenants`, `fenec_http::node_stream` on the node, `node_archive` the
archiver). A tenant's own stream is a connection and a thread on the node
for as long as it runs, one of `--max-connections`, and holds its tenant
open: archived so, a node of 1 000 tenants of ten rows and one of 64 MB held
1 016 threads, 4 011 descriptors and every tenant open for good, against 15,
27 and the 10 written with no archiver -- and at the default 100 connections
could not be archived at all. `POST /_replication/tenants`, with the
replication token, is one connection and one node thread for every tenant:
its body is where each archive under the root stands (`<tenant> <since>
<history> <image>` a line), the node says hello to each tenant it holds (an
image where the place does not lead to its writes) and `G` to each listed
one it does not, and every message is the tenant stream's led by its name.
It sends a tenant's writes from its feed while it is open, and nothing while
it is closed, whose file does not change. It is a sender on each open
tenant's feed as a replica's stream is -- put there as the tenant opens
where its client stands where the file does (`NodeStreams::attach`), and on
the tenants open as it connects, before it reads which tenants there are
(`begin`: read first, a tenant made between was in neither) -- so a `--sync
always` write waits for it, and `Tenants::close` refuses while a sender has
a write left (`Feed::unsent`), syncing a dirty tenant first: closed, its
feed went with it and the writes came again as an image. One stream for
every tenant holds no tenant up for another: its sender carries a flag
(`Sender::held`, `Listener::hold`) set while the stream does what can block
-- a closed tenant read or opened to be looked at, a tenant's lock waited
for (`Listener::read`: waited for unheld, a tenant write-locked as the
stream came to judge it held another's next durable write 1.014 s), an
image's snapshot and its sending, a socket the archiver reads slower than it
fills (written non-blocking, `Out`, the flag set before a write that would
block) -- and a durable write waits for no sender held so
(`Sender::waited_for`); a stream that starts behind, at an image or on the
tenants open as it connects, is lagging until it catches up, and an image
waits for its tenant's disk among the stream's other work (`Pending`, looked
at again every 50 ms) rather than in `DURABLE_WAIT`'s 30 s. Its feed is
among the stream's while it waits, where a close takes it off
(`NodeStreams::detach`) and an open puts another in its place, `attach`
dropping a feed of an earlier opening, and `send_pending` sends only while
it is still there, taking the tenant again otherwise: an image pending
across a close and an open put the dead feed back once sent, the stream read
nothing from it again and the archive stood still -- the node at 8, it at 5
-- until the archiver connected again. Held up the replica's way, every
tenant's next durable write waited out `SENT_WAIT`, 1.008 s, while the
stream opened another tenant. The syncer of `--sync <ms>` and a close sync
waiting for no stream (`replication::unwaited`), since no client's answer
waits on them: waiting, a pass took 5.06 s over five tenants beside a stream
busy elsewhere, the writes of the pass unsynced meanwhile, and a close held
its slot's lock 1.01 s. So an archive of a node is synchronous only while
the stream keeps up, and under load it is not: a write answered while the
stream is held up or behind is on the node's disk and not yet in the
archive, sent once the stream comes back to its tenant -- at worst inside an
image -- and missing from it if that disk is lost first; a replica's own
stream keeps a write's answer waiting up to a second instead.
`tests/node_archive.rs` holds a tenant's open, a tenant's lock, an image's
disk and a full socket each beside another tenant's writes, and fails with
the flag, the held lock wait, a blocked write's hold or the pending image
taken out (a write beside an archiver that stopped reading waited 1.014 s)
-- the unwaited sync is `replication`'s unit test's to hold
(`the_syncers_durability_waits_for_no_stream`), since beside a stream held
up a sync waits for none either way; it asks `Feed::waited_out`, the durable
writes that waited out a stream, rather than a clock, which a runner slowed
by its neighbours fails. Which tenants have news is told: a feed tells as
its writes reach the disk, it starts over or closes (`Feed::tell_with`, held
strongly: a `Weak`'s upgrade was a compare-and-swap on one count every
tenant's sync shared, 139-141 ns a tell from 16 threads against 0.20-0.24),
and a tenant made, installed, deleted, promoted or made to follow touches
the stream; one released while the stream is busy with it (`Next::Gone`) is
looked at again until it is gone or back -- a tenant made again under the
name, writing nothing, gave no notice more -- and the directory is read
again every 10 s for a file put there by hand. With no stream connected a
sync pays two loads, 13 ns a durable advance either way. A closed tenant's
place is where it closed in this process (`Tenants::closed`) or read from
its file (`Tenants::peek`, `open_read_only`), where opening to look took 6.3
ms a tenant, the open fsyncing the file; a closed tenant the stream is told
of is read again, since a holder of its database may have written it after
the close, and one with a non-empty sync log beside it (Linux, after the
machine went down) is opened, the log holding durable writes the file lacks.
The feed keeps the lineage the database last wrote, read off the history
records `Tee::append` passes, and the stream sends it (`L`) before the
writes after a promotion: a promoted standby's archive goes on with no
image. The archiver keeps `<root>/<tenant>/` as `fenec archive` keeps one,
through the same `archive::Writer` the single archiver now writes with; a
segment file is open only while its tenant writes (a thousand would pass a
macOS shell's 256 descriptors), the `history` file is written only when it
moved (a thousand fsyncs a reconnect otherwise), and `archiving.lock` keeps
a second archiver off the root. A tenant the node says is gone -- deleted,
or moved to a node whose archiver takes it up from its image on the same
history -- keeps its archive, and `presence` notes `gone <ms>`, then `back
<ms>` if it returns: a restore to a moment it was away leaves it out, as one
deleted, where forgetting the time away gave the copy from before it left.
The times are the node's clock, as its writes' are: `G` carries when this
process deleted the tenant (`Tenants::departed`, the last 65 536, kept only
on a node that replicates its tenants and noted once the file is gone: noted
before it, a removal refused had the archive read the tenant gone and back
62 ms later) or else when the stream saw it go, and `back` is the node's
hello -- stamped as the archiver heard, a moment after a delete still
restored the tenant while the stream was busy or the archiver away. A tenant
deleted and made again while the stream was busy is told gone at the delete
before the new copy's hello (`Session::left`), and a line moved aside for a
copy heard of alone, the archiver away meanwhile, is noted gone by that
hello at the latest; `away_at` reads the last line noted by a moment, a
clock set back leaving the times out of order. An image at or before the
archive's end on a history that does not lead from it
(`archive::Took::Diverged`: restored to an earlier moment, or another tenant
of the name) would have landed among changes of the same numbers --
`File::create` of a segment of the same name wrote one over -- so the single
archiver refuses it and the node's moves the line aside to
`<tenant>.<history>` and begins a new one. `restore --tenants` to a time
takes, of a tenant's lines, the one that began last by then
(`node_archive::lines`; `Archive::began`, its oldest image's time, which a
`prune` letting that image go writes into `began` first -- written with the
first image it was an fsync more a tenant of the stream's first pass), and
refuses a moment `--keep` let go of (`Restoring::LetGo`, exit 1) rather than
pass the tenant over as never archived; `verify` and `prune` run on every
line, and `Archive::verify` names a gap an image alone ends. One tenant's
archive that does not read is left for an operator and the rest go on.
Measured with `make node-archive-bench` (1 000 tenants of ten rows and one
of 64 MB, closed when idle, ten written ten times a second from before the
archiver connected and for 20 s after, under `--sync always`, in turns): the
node held 16 threads, 29 descriptors and the 10 written open, 3.9-6.0% of a
core against 4.7-5.1 with no archiver and 7.0-8.0 a stream each; the
archiver a thread and 21 descriptors against 1 001 threads and 2 021; the
writes while the first images were taken, the 64 MB one among them,
14.5-14.7 ms p50, 32-35 p99 and 43-116 at most, where a stream each held
them 176-600 p99 and up to 1.1 s, main's build alike (86-351, up to 994 ms);
after them 12.4-13.8 p50 against 11.8-12.1 with none (`F_FULLFSYNC`); a new
archive's first images, a tenant at a time, 14.8-15.2 s against 11.2-15.5
for a thousand streams; after a restart 2.2-4.6 MB and no tenant open
against 121-157 MB and every one, all 1 001 said hello to in 0.29-0.33 s.
Under `--sync 250` (`node_archive_passes`: eight tenants written every 5 ms,
eight opened, written and closed in turn, one of 32 MB closed) the syncer's
pass took 52 ms p50 beside the node's stream, 91-168 at most -- the
archiver's fsyncs on the same disk -- against 31-33 with none and 65-86 a
stream each, and a close 4.0 ms p50, 15-33 at most, against 3.8-4.0 and
12-25 with none. With no archiver and with a stream each, every figure
stayed main's in ABBA turns (the pass 31.0-31.4 and 67-73 ms p50 on main, a
close 3.6-3.9). The pending feed, the lock held up and the departures moved
nothing, four turns a side against the build before them: the writes while
the first images were taken 13.9-15.3 ms p50 and 26.7-32.9 p99 against
13.9-14.9 and 29.5-30.6, the first images 14.8-16.8 s against 15.1-16.8, and
with no archiver 11.5-12.1 ms p50 against main's 12.0-12.1. `make
replica-bench`, four turns a side at a load of 10-15 from other work:
`--sync always` lag 0.10-0.15 ms p50 against 0.10-0.11, catch-up 11 128-13
614 writes/s against 11 384-13 857, a restore to the end 7.87-9.09 s against
8.51-9.05, each pair inside its turns' spread -- a replicated write meets a
byte compared in `Tee::append`, a thread-local in its flush and a flag in
`wait_sent`. A node's request path (`make shard-bench ARGS=latency`, a
tenant's read straight to the node) stayed 21.5 us p50 against main's 21.6,
eight turns a side, with the stream's handler out of line and cold (inlined
into the connection loop, four turns a side read 21.5-22.0 against
15.7-21.5: main ran in a 15-16 us mode three times in twelve, this build
never). Nothing below `fenec-http` changed.

## Change data capture

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
`POST` at the last write on disk or the `since` it names -- read from
"now" each time, it missed what came between two reads -- and moved by a
`PATCH` of `since` alone), each write at least once. A `POST` of a
consumer that is there writes nothing and answers 200 with where it
stands, for no `since` or one at or behind it: that is where a new one
starts, as Kafka reads `auto.offset.reset` only for a group with no
committed offset. A `since` ahead of it is 409 naming its place
(`cdc::ahead`): it is a commit sent as a `POST`, as every commit was
before, and answered 200 an older client -- the Python SDK's, the recipes
of the time -- read the same page for ever, told nothing (a review ran
50 identical pages). The `POST` was the commit too, and one with no body moved a
consumer that was there to the last write on disk: a relay or a follower
making its consumer at each start, the natural way to write one, skipped
every write made while it was stopped -- found writing the recipes, whose
pages made a consumer only where `GET /_changes/consumers` listed none.
Answering a consumer that is there 409, as Redis's `XGROUP CREATE`
answers `BUSYGROUP`, was rejected: every caller takes that error for a
success, and one making its consumer from the first write (`{"since":
0}`) would fail every start but the first; a caller that must know it
made the consumer reads 201 from 200. A `PATCH` of a consumer that is not
there is 404, never a make, so a commit racing a `DELETE` makes none
again. Both run under the write lock (`cdc::changed`), which refuses each
on a following database -- its consumers are its primary's, and a `POST`
of one that is there, writing nothing, had passed there, leaving a relay
pointed at a replica to fail at its first commit -- and wait for the file's
durability before the answer even where they wrote nothing, so a consumer
another request made a moment before is not answered as there before its
fsync. Each answers where the consumer stands after it -- a commit past
nothing but cursors writes nothing and leaves it where it was, which
reads the same writes, and answered as sent it named a place no list and
no later `POST` found -- and `Fenec-Seq`, so a read sent after it
(`Fenec-After`) to a replica finds the consumer as it left it. A `since`
is taken up to the last write the database holds (`change_seq` under the
lock), as a read's cursor is (`Tail::Ahead`), not the last on disk: the
feed drops chunks by size whether or not they are on disk, so a 410
names `oldest - 1`, which may be ahead of the disk, and a commit held to
the disk refused the one place the consumer was told to go on from. Taken,
the commit's row lands after it, a read from it waits for the disk, and a
crash hands the writes after it over again, never skips them. On a tenant
node a consumer's write is the tenant's (`consumer_write`): held against a
move's freeze (`Tenant::enter`) and told to come back (503) while the
tenant is frozen or its lease is out, then asked of the lease as it is
answered. Run past the gate, one sent during a move landed on the source
after its export and was lost at the flip, and a make lost so was made
again later at the last write on disk, past every write in between. The
stream leaves `_consumers`' own writes out, the
cursor going past them, and a `wait` waits on past them rather than answer
nothing at once, while a commit to where nothing but cursors were written
since the consumer's place writes nothing (`unmoved`, the change ring's
`changed_collections_since`): a sink that committed every answer
committed its own commit's empty answer, 30 000 fsynced writes in a few
idle minutes. A feed kept for it alone has no token (`Replication`'s
token is an `Option`: an empty one matched an empty `Bearer`). Keeping it
cost nothing measurable, 16 400 single puts a second over HTTP either way,
and 9 000 rows of 128 dimensions read back at 283 000 a second.

## Fenec-After

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
Python's `http.client`, parsing one more, went 18 200 -> 17 900. Only a
request `handle` would take past its token waits (`admitted`: the MCP
endpoint's own checks where it goes there, `authenticate` elsewhere, and
before either `route_tenant`'s token and tenant); a refused one goes on to
`handle` at once, refused and counted by `audit` as any. On a single
database the wait ran first: a guessed token held a thread for up to 30 s,
enough of them every one `--max-connections` allows, and its 504 said
where the database stood. Every other wait already came after its token
-- a held claim is planned only past `authenticate`, `/_changes` and a
subscription ask first, a replication stream its own token, `/_admin/` the
admin token -- and the router forwards what the node refuses at once
(`tests/token_first.rs`, fenec-shard's
`a_refused_token_waits_for_nothing_through_the_router`: each refusal asks
30 s for a write made only once it is in, so one that waited would be the
wait's 504, never 401). A request without the header asks nothing more:
`make requests-bench` in ABBA turns against main kept every p50 and its
rates within 0.5%. A subscription and `/_changes` wait the same way: the
stream in `sse::serve` once its token, its tenant's stream slot and the
database's `--http-max-streams` slot are taken, and before the shape is
read -- a collection the client made may not be on the copy yet -- and
before the head, so its 504 is a read's and a router's replica path sends
it home; `/_changes` once its token, a scoped token's 403 and a feed are
asked, holding a stream slot as its `wait` does. Both came before the
wait: on a single server `GET /t/changes` sent `Fenec-After: 999999` and
`Fenec-Wait: 300` was seeded at once where `/query` was 504 after 0.31 s,
and through a router a stream marked by Go's `Read()` or .NET's
`AsRead()` -- a router sends a marked request near whatever its route --
seeded from a read replica behind the client's own write, or left on the
branch a failover abandoned, and a page's `Fenec-Seq` behind the write
read as every write there is. A stream resumed with `since` waits too:
`since` is the stream's cursor and the header the client's position, and
`since` alone asks no wait -- a primary that lost writes in a failover
never reaches a cursor of the old branch in its history, and reseeds as
before. The server writes no `id:` line, so no client sends
`Last-Event-ID`. A page waits for the change to be in the database, not
on its disk: its `Fenec-Seq` is never behind the write, and the write
comes on a later page once an fsync covers it (`/_changes`' own `wait`
waits for writes after `since`). A marked stream on a copy that has not
caught up, or refused another lineage's primary, is 503 before any of
it, as a read is, and one open as the copy deletes itself ends with the
hub (`release`). Go's `Subscribe` and .NET's `SubscribeAsync` take a
stream's 504 near as a read's (`toldBy`, `SentHome`): home's position
taken and the stream opened home unmarked -- an error, every stream near
after a write lost in a failover was refused for good. `Hub::after_waiting`
counts the requests in the wait, what the tests wait on before the write
comes: `tests/replication.rs` holds a stream, a stream resumed and a page
on a replica whose follower is halted, each seen waiting, then served
with the write, and 504 past the wait; `tests/token_first.rs` each refused
at once with the header, and a good token's stream answered by the wait
(its `ask` reads to the end its head names, so a stream served instead
fails rather than hangs on its keep-alives); fenec-shard's
`a_marked_stream_after_a_write_waits_on_the_replica_or_goes_home` a
stream and a page going home past the replica's wait, and a stream
answered by the replica once it catches up, home cut off; Go's
`TestAStreamAfterAWriteWaitsForItOnAReplica` and .NET's
`AStreamAfterAWriteWaitsForItOnAReplica` a real replica's stream after a
write it cannot hold -- its link to the primary through a proxy the test
cuts -- answered 504, and seeded with it once healed -- each failing with
its wait taken out. A stream's head names where the database stood
as it opened, as an answer does: `Fenec-Seq`, the change its seed is at
or after, and `Fenec-History` where the database has a history of its
own -- read under the lock the shape is parsed under, no lock of its own
-- so a client re-running its reads near while the stream stays home
sends them after an event's `seq` of that history, which a number alone
does not name across a failover (`tests/replication.rs`, a primary's and
its replica's head; `tests/changes.rs`, the root's naming none). The head
carries every answer's CORS headers (`cors::push_into`): it wrote the
origin alone, and a page on another origin could read no header of it.
Without the header a stream and a page look for
it once, as a read does: `make requests-bench` in three ABBA quartets
against main, a row by id 0.021-0.022 ms p50 against 0.022, a put
0.021-0.023 against 0.022-0.027, eight clients' rates inside main's
spread (by id 136-147k a second against 126-145k); a subscription opened
to its seed of a row, its head naming its position, 1 000 opens a side in
each of four ABBA runs a build: over a database with a history of its own
245.2-251.2 us p50 against main's 250.6-254.6 (the root's, no history
written, 248.1-251.6 against 251.0-254.4), and sent after a change the
database holds already 247.8-252.7.

## Sealed archives

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

## follow

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
an error it cannot wait out stops the server rather than leave it serving
a mirror that no longer moves -- through the one shutdown, exit code 1
(`durability::stop_with`, [durability.md](durability.md#the-one-shutdown)). This is why the PostgreSQL client is
`fenec-wire`'s, below the importer: in the server it made the importer
depend on the server, which could then not run it. A message's length is
held to PostgreSQL's gigabyte (`proto::MAX_MESSAGE`, its MaxAllocSize)
before anything is allocated, and under it the body's buffer is the
length believed, allocated so its failure is an error (`proto::zeroed`,
as the router's `zeroed`, and for its reasons: no copy at any size, a
lying length costing the pages its bytes reach): a length from the
server, or from anyone on the path (fenecdb speaks no TLS), that a system
refused -- one that does not overcommit, Linux's heuristic on a machine
smaller than the gigabyte -- aborted `fenec import`, and `fenec-server
--follow` with everything it served, and is a broken stream now, which
the follower connects again after. The counts a message carries are held
to it too: a column count is an Int16 never negative, read unsigned
(`be_count`: read signed, `0xFFFF` sized a `Vec` by `usize::MAX` and
panicked), and a TRUNCATE's relations are given room for what the
message holds, where its count alone asked for 16 GiB. The reader is the
code it was, `calloc` either way, the loop reading messages the same
instructions in other registers once the refusal's message went out of
line (`refused`: formatted in place, the length was stored to the stack
at every message): over messages in memory, twelve runs a side in ABBA
turns, 120 B 30.2 ns a message against 30.3, 3 500 B 248.4 against
250.0, 64 KiB 8.53 us against 8.61, 2 MiB 58.5 against 58.6. A subscriber hears a PostgreSQL
commit 0.09 ms after it returned at the median, a row inserted with a
vector under HNSW 0.19 ms, p99 4.0 and 3.3 (`make mirror-bench`; the code
before the pg wire went gave 3.7 and 3.7 the same day); a server killed
while the table was written to held every row 370 ms after it started
again. The
follower adds 178 KB to `fenec-server`.
