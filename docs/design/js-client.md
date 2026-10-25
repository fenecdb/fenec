# The JS client and Cloudflare

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *The JS client and Cloudflare*: the module and the client entry, live queries, Durable Objects and their alarms. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## The module and the client

**One module and a client entry, by where the queries run, and a test
build.** `@fenecdb/web` ships `fenec.wasm` (every index and the schema
check, 244.1 KB brotli) and `@fenecdb/web/client`, so a user has two
clear choices: queries on a server through the client entry and no
module, or a database in the page or a synced replica through the full
module. `fenec-lite.wasm` (no index, no schema check, 173 KB brotli; `make
wasm-lite`, as `make wasm FEATURES=language SCHEMA=0` now) was shipped
beside it and is a test build now -- built in CI, packed and released by
nothing -- because it holds the invariant above: a build without an index
opens a file that declares one, and `web/fenec.test.js` hands files
between it and the full module both ways. `fenec-min.wasm` (`make
wasm-min`, no feature at all) is the same kind of test build. The client entry is `web/client.js`: the builder (`builder.js`) and the HTTP client
(`http.js`) re-exported, the two modules `fenec.js` imports beside its
glue, persistence and sync. The entry's worth is that it cannot pull in
the engine, sync or storage, not the bytes: a `connect`-only app never
fetched the `.wasm` through `fenec.js` either (only `Fenec.open` does),
and bundling `connect` and a builder query's rows is 8.8 KB brotli
through the entry against 11.7 through `fenec.js` (5.7 against 8.2 when
the entry was made, 7.5 through `fenec.js` before `FenecHttp.live`), whose `Fenec` class a bundler cannot drop -- its static
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
of 24 run. On a client reading near (`reads: 'nearest'`) the streams stay
home, unmarked, and every run -- the first rows too -- is a read near, sent
after the furthest of the client's position and the live query's own
(`AFTER`, kept apart: another client's change is no write of this one's,
which its other reads near would wait for): the furthest its streams named,
each event's `seq` of the history the head names (`Fenec-History`; the
seed's `seq` is at or past the head's `Fenec-Seq`), ordered as the client's
(`later`), so a stream opened again after a failover names the later term
and the old primary's moves nothing -- and the furthest its runs' answers
named, since a marked `/query` read's answer carries where the copy that
answered stood (`cfg.marked`, `At::of` under the read lock it reads under,
at the pin for a pinned read; every SDK takes a marked answer's position
for nothing of the client's): a run the router answered from home, past a
change whose event was still on its way, showed rows the next run sent
after the last event alone could have a replica behind answer without --
`[a,b,b2,c]` -> `[a,b,b2]` -> `[a,b,b2,c]` in review. A run waits a second
on the replica (`Fenec-Wait: 1000`), not the node's five: a replica has a
write once home's fsync covers it, 283 ms behind at most under `--sync 250`,
and a link later, and home answers in a region's round trip; a replica's 504
rests the copy at the router (above), so a replica cut off costs a live
query one second each `RESTING`, not five each change. The second assumes
a primary syncing at `--sync 250` or faster and links under about half a
second: at `--sync 1000` and up every near run misses it and is answered
home (`fallback` rising, the rows a round trip late), right but slower than
reading home, which `sharding.html` says; not an option of `live`, since a
deployment that slow reads home. Home's 504 -- the change lost in a
failover -- sets back whichever position the run was sent after and asks
near again after home's, home unmarked only where that fails too, since an
unmarked answer names nothing -- and so a run after that one may show rows
from before it, left so: it takes a failover that lost writes, whose lost
write the rows lose anyway. A head naming no history -- a server from
before heads named one, or a database never replicated -- sends the runs
home until every stream's head names one again (`bare`, by collection: set
for good, a stream reopened after an upgrade left the live query home):
term-0 positions never pass the client's. The rows are
never older than a change a stream told of or a run showed, and a stream
tells of every change home takes. The polls stay home: a tag is a change of
the copy that answered, and a replica holding that change of its history
(`Fenec-After`) judges it as home does up to where it stands -- behind,
`unchanged` reads a tag past its change as changed and runs the query, rows
older than those shown -- but nothing in a poll names a change home took
since, as a stream's events do, so a replica behind or cut off from home
would answer 304 to a write home holds. `web/fenec.replicas.test.js` holds
it to two routers, two nodes and a read replica that follows its primary
through a proxy parking what the primary sends (`parking`, which counts
what the routers ask the replica's node too), the tenant's `streams` quota
set so the replica's node counts a run held there (`usage.streams`): the
first rows and a change's run held on the replica, answered by it once
released (`fenec_router_replica_reads_total`'s `replica`), one never
released sent home after its second, and the changes after it home at once,
the replica asked once for three (`fallback`), the stream unmarked --
failing at once with the streams' position, its history or the mark taken
out, a stream marked, or the router's rest; runs through the home region's
router at a tenant whose one stream its live query holds answered, failing
with the slot taken first; and scripted tests: two streams' seeds and
changes, one behind another's, the client's own write further, a stream
reopened on the next term, the old primary's change ignored, home's 504
asked near again after home's position and home unmarked after a second, a
marked answer moving the live query's position and not the client's; a
replica behind home answering the run after a home answer -- failing with
answers' positions left out as the review's rows did; a head with no
history and a poll going home, unmarked. `useLiveQuery` passes the client
through and needs nothing. An app's bundle of `connect`, rows and `live`
went 29 684 -> 30 124 bytes, 9 728 -> 9 891 brotli; `http.js` 26 412 ->
29 298, 7 787 -> 8 586 brotli, which took the client the docs quote to 194
KB, 51 brotli, and the studio's first load to 66 KB gzipped.
`db.subscribe(collection, shape, onEvent, { onError, onState })`
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

## Persistence in the page

Not an invariant: the part of CLAUDE.md's *Architecture* on how the browser client keeps a database, moved here word for word, measurements included; CLAUDE.md keeps the journal's and the two-copy image's rules and says that the image and chunks can be sealed.

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

## Durable Objects

**A Durable Object keeps a database as a file would** (`integrations/cloudflare`,
`@fenecdb/cloudflare`). A Worker imports a `.wasm` compiled, and `Fenec.open`
takes the module so -- it read `.instance.exports` off what `instantiate`
gives a module, and the engine could not start in a Worker at all.
`persist` writes an image into `ctx.storage` and then only the writes since
(`journal`/`drain`), until they outgrow half the image and 64 KiB. An object
is billed by the row written ($1 a million), and a SQLite-backed one -- the
only kind a new account makes -- has `storage.sql`: there a generation is a
table, `<key>:<gen>` (`id INTEGER PRIMARY KEY, image, data`: an index is a
row written more), the image's rows under the 2 MB a row takes (`ROW`),
then a row a flush of the writes, and nothing beside them says where the
database stands, so a write is one row. A new image is written and the
tables before dropped in one `transactionSync`: workerd counts a table made
as two rows, one dropped as none and a `DELETE` a row each. The key-value
layout -- values under 128 KiB, a record naming the whole generation
written last -- is kept for an object without `sql`, and read through
it alone: a database an earlier version kept in values on a SQLite-backed
object is moved by restoring through the key-value methods and persisting
(README), not by code. Before, a single-row write was a value, the record
and the example's `setAlarm` (a row too), 3.4 rows; a checkpoint of
10 000 x 128 (5.7 MB) 101 rows and of 50 000 x 128 (27.6 MB) 479, its
128 KiB values and a delete each of the generation before. Now one row, 5
and 16 (`make cloudflare-bench`'s rows.mjs, every storage call counted
under `wrangler dev`). A flush goes out at once unless the one before is
not yet durable (`storage.sync()`); the writes that land meanwhile -- a
Durable Object takes the next request in while one waits -- go out after
it as one: 0.52 rows a write from 8 writers at once, 0.15 from 32, every
`persist` resolving once its own flush is durable and a read's once what
it may have read is. Waited out with a timer, a turn of the event loop for
requests to land, it cost a lone write 3.2 ms p50 against 1.4; this way
2.0-2.3 against the 2.4-3.1 before. A flush that fails leaves the storage
as it was and every `persist` on that database rejecting after: the
example lets it go and the next request restores, where it answered 400
and kept the write in memory for the next image to store. The example
opens and restores inside `blockConcurrencyWhile` (a cold object sent
requests at once could open an engine each, appending under the same
indices; `wrangler dev` never showed it in a burst of 128),
and keeps its database from its alarm (below); `checkpoint` writes nothing
until the writes since reach `{min}` (64 KiB), and then a new image: a restore takes the graphs in it, where the
vectors written after the last image are linked one after another in the
browser module's one thread -- which no longer costs a start much: the
first `near` over 10 000 x 128 after a restart 39 ms as written, 47 after
a checkpoint, 50 000 175 and 157, where it was 792 and 1 455 against 36
and 84 when first measured (`make cloudflare-bench`, under `wrangler dev`,
which runs from the example Worker's own `--config`: below the repository
it took the site's `wrangler.jsonc`). A restore copies each row or value
into the allocation the module keeps the image in as it reads it
(`Fenec.loadParts`, JS only): read whole into a `Map`, joined, and copied
in, a 27.6 MB image held 60 MB of JavaScript beside the module's 42 at
the peak, now 4 (memory.mjs, in Node), and the start of 50 000 rows went
186-196 -> 175 ms. A Worker's isolate has 128 MB with the module's memory,
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

## Alarms

**An object's alarm keeps its database** (`maintain`, `schedule`, `due`
in `integrations/cloudflare`). A server sweeps `@ttl`, compacts and keeps
its graphs on threads; an object has one alarm, a row written each time
it is set and a wake-up -- a restore, if it was evicted -- each time it
rings. The example set one after every first write and checkpointed past
64 KiB of writes, and nothing deleted a row past its time: read past, kept
for good. `schedule`, after a write, sets the alarm only once something is
due -- a row that will expire, an image owed -- `quiet` (5 s) after the
write at the soonest, leaving one set no later than `every` (60 s) after
that, and asks nothing where one rings that soon (`due` is 8 us a write in
the module, 29 beside 100 000 rows that expire, a put 5); a write that
brings nothing due and every read set none, so an idle object costs
nothing. `maintain` deletes the rows `grace` (60 s, the server's `GRACE`,
so a reaper reading `expired()` as often sees each) past their time,
10 000 a call in statements of 1 000 -- `del c where expired() and f <= $1
order f limit $2`, which walks the `@ttl` index to the batch, so the module
took no export and not a byte (`Database::expired` and `sweep` are native
only): 0.46 ms a batch over 100 000 rows, an alarm's 10 000 holding the
object about 5 ms -- and sets the alarm again at once past the bound;
writes a new image once the writes since reach half the image and 64 KiB,
or half of what storage holds is dead and a mebibyte, which the writes'
size alone does not see (the module's dead record bytes less those it held
as the stored image was taken, `deadAtImage`: an image holds the live
records alone, and every write after lands in both), so an alarm woken to
sweep stores its deletes, one row, and no image -- under a mebibyte an
image of a churning 70 KB was three rows a sweep where the deletes were
one; and sets the alarm for the next row's time and grace, no sooner than
`every` after the sweep, or none, the alarm that rang forgotten either way
(kept, `schedule` took it for one still set and set none again). A failure
to set it -- `getAlarm`, read on every alarm that wakes an evicted object,
or `setAlarm` -- is `maintain`'s `error`, its work stored, never a
rejection: thrown, the example's handler let the database go as for a
storage failure (below). The names its statements write are the lexer's
(`\p{Alphabetic}`, then `\p{N}` too): ASCII alone, a `müşteri` was never
swept. A year past 9999 is written `10000-01-01T00:00:00Z` and one
before year 0 `-001-01-01T00:00:00Z`, four wide with its sign, which
`Date.parse` reads as `NaN`: the alarm was set at `NaN`, and a row of year
-1 past its time passed its whole collection over. Both are read in
ECMAScript's six-digit form; a time past any date JavaScript has is taken
as due now for a row past its time and passed over for a living one. Taken at
a quarter of the image, rewrites of 1 000 128-dim rows a request, a row
each, brought an 11 MB image, 8 rows, every 6; at half, where `persist` folds, the
images are as many as before, and a request folds only past the whole image
once `schedule` ran (`BACKSTOP`), so the alarm takes them and no request
waits. Under `wrangler dev` (rows.mjs, every storage call counted, in ABBA
turns against main): 30 bursts of 20 single-row writes over 10 000 x 128,
a second apart, 600 rows against 651-652; 12 bursts of 500 rows 19 against
100; 20 000 rows past their time deleted in two alarms, a row each. The
module never gives memory back and holds every record version written
since its open (20 000 x 128 restored held 18 MB, 74 after its titles were
set thrice and its vectors once): once its dead record bytes reach half
of what it holds, between 4 and 16 MiB, `maintain` makes sure storage holds
an image without them and answers `reload`, once a database, and the
example restores a new module -- 50 000 x 128, three in five expired, held
42.7 MB through two sweeps, 51.6 at the image the third owed and 19.2
restored (memory.mjs, in Node). Every database the example lets go -- a
reload, writes not stored, a `maintain` that failed -- goes one way
(`#retire`): calls arriving wait while those holding it finish, a write
on its way to storage among them, then it is `release`d -- nothing of it
stored after, resolved once its flush under way has settled -- closed, and
another restored. `release` remembers the database (a `WeakSet`), not only
its state: one never kept, or restored from empty storage, had none to
mark, and a `persist` after made one afresh and wrote an image over the
generation stored; `restore` into a released one is refused, where it
made a state anew. Let go of at once, as the alarm's handler first did when
`maintain` threw with storage sound, it lost answered writes: a request's
flush waited to be durable and another's waited behind it, the next
request restored a second database and wrote id 3, the first then stored
the waiting write as id 3 too, and the next restore read the first's. And
a refused `persist`, `checkpoint` or `maintain` on a failed or released
database answers only once no flush of it is under way, so a caller that
drops it and restores -- the README's alarm, a request's write not stored
-- reads all it stored and is alone: refused at once, it answered while a KV
flush still put its pieces, which would land after that restore. `example.test.js` drives the
example's class in Node over the storage stand-in (`module.registerHooks`
giving it `cloudflare:workers` and the compiled `.wasm`), holding a flush
at `sync` to put the alarm between two requests' writes; it fails on the
code before, and with the wait for the holders taken out. A `compact` in the module rebuilds every
graph holding tombstones and builds its image whole, and gives nothing back.
A restore reads its generation's sums to their end: left at the first row,
the statement held the table and a new image's `DROP TABLE` was refused
under node:sqlite. The first `near` after a restart, a lone write's p50
(2.00-2.07 ms against 1.96-2.35) and a restore's memory moved by noise
alone. `make cloudflare-test` holds the rules to the fakes -- storage
failing after every operation of a sweep's flush and of its image included
-- and the example under `wrangler dev`, on the port it says it bound
(`--port 0`, `Ready on`; dev.mjs, which rows.mjs and coldstart.mjs start
it through too): an idle object setting no alarm, 350 rows swept over four
alarms and kept across a restart, an image by the rule and none without
it, a reload. wrangler is started with no colour and its line read past
the escapes besides -- under FORCE_COLOR the address sat between them,
and the harness waited for good -- a start that ends before the line
fails, and an `after` hook ends any wrangler a test cut short by its
timeout left, which held the test process open.

## Live queries

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
