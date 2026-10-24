# Indexes

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *Indexes*: `@unique`, hash buckets and tables, `@ttl`, what reaches an index, bound filters, `@sorted`, composite indexes, lazy builds, `--warm` and `@geo`. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## @unique

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
it and asks nothing: the primary did. The clash names the value as the
row holding it stores it, read from that row (the refusal alone pays it):
under `nocase` the spelling written is not that one, and a sign-up as
`SAMI.lindqvist@...` was told the row held `SAMI...` where it held
`sami...`. A scoped token is told the field alone (`access::told`, in
`within`): the clash names the other row's id and echoes its value, which
told alice that bob's profile existed, where, and what it held.

## Hash buckets

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

## Sharded tables

**A hash index's table grows a 256th at a time, and a text index's
terms too** (`maps::Sharded`). A `HashMap` that is full moves every key
into a table twice its size in the insert that found it full -- under the
write lock, every request waiting -- and a `@unique` field holds a key a
row: `make recon-bench`'s journal passing 1 835 008 entries held one
transfer and everything behind it 120 to 137 ms in every run, and `make
growth-bench`'s 4 million puts one at a time took 7, 19, 49, 109 and
235-267 ms at each doubling from 229 376 keys. A `@text` index's terms were
the same map, and a field whose rows bring terms of their own -- an id, a
code, a name -- doubled it as often: 9-10, 23-25, 55-59, 125-132 and
276-324 ms over the same doublings (`growth-bench`'s `text`, four new terms
a row to 4 million). Past `SPLIT_AT` (114 688 keys, a table of 2^17
buckets' worth) the map is 256 maps, each growing on its own: the longest
put is the split's, 2.5-3.6 ms for a hash index's keys and 4.9-5.3 for a
text index's terms, against 6.2-8.2 and 8.5-10.6 while the split hashed
every key again. A text index's terms are a table of their own since,
split the same way (below). Each key is kept beside its hash (`Hashed`), which its
table takes as it is (`Pass`): a lookup is one SipHash under the map's own
random keys, and the shard is bits 49 to 56 of it -- below the seven a
table tags its buckets with: by the top byte, every key of a shard had one
tag and a probe read every key it passed. Picked by an `Fx` of the key, the
shard then hashing it again with keys of its own read from the shard, a
lookup in a loop over a million keys took 94-135 ns and over four million
121-144, against 68-87 and 83-93 now, and 33-42 -> 24-34 under the split;
an index built after an open is 15-25% faster, its tables growing with no
key hashed again. The cost is 8 bytes a slot: 48 -> 56 a hash index's, 80
-> 88 a text index's then. A put's p50 is as it was (1.08-1.29 us), and so is
`match` (a new term 1.05-1.29 us against 1.09-1.37, three 1.37-1.65 against
1.41-1.72). Rejected:
one table taken into a new one 4 096 keys at a time, looked up in both
meanwhile -- lookups 163 ns, since a map that stops taking keys never
finishes its steps, and builds a third slower. The browser module keeps
the one map and its `Fx` (`cfg`), byte for byte the size it was.

## @ttl

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
user: its purge on the write path went. A subscriber hears a row reach its
time at that time (`sse::Expiry`): told at the sweep, a minute and its
grace later, a courier whose phone went quiet stayed on a dispatcher's map
73 s after it had gone from every read. A stream of a collection with
`@ttl` works out when its next row expires (`wakes_at` with no filter, a
step of the index, 1.3 us over 100 000 rows, again only after a change to
the collection or a look), sleeps until then if no write comes first --
`Hub::wait`'s timeout, writing no keep-alive for it -- and reads the rows
that reached their time since it last looked: `expired()` and the field's
window, the shape ANDed in, every row of the collection for an unscoped
shape that holds nothing (`FenecHttp.live`'s, told of any write to it),
sent as deletions at the change the stream stands at -- a scoped stream's
held to the ids it was sent, a quiet one's emptied -- and a resumed stream
tells every row past its time not yet swept. The sweep's delete comes
after, a deletion of what is gone. About 10 ms after the row's time in the
test, which holds the row still in the file as `expired()` once heard, and
a scoped stream silent at another user's row's time. The two features cost
the browser module 16.4 KB, 5.7 KB brotli -- 18.7 KB more while a ttl printed back
through a float, which brought the standard library's float formatting.
`expired()` cost it 561 bytes, 98 brotli.

## Index reach

**Only an `and` chain reaches an index** -- equality (`=` or `in [..]`) over a
`@hash` field or over `id`, comparisons over a `@sorted` field, the leading
parts of an index over several fields and a radius or a box over `@geo`
(both below). `in` is a set
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

## Bound filters

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

## @sorted

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
The structure is a sorted `Vec` of chunks of at most 256 entries, not a
`BTreeSet`, which made the browser module 75 KB larger, each chunk found among
`firsts`, the chunks' first entries side by side: searched over the chunks'
own, each probe was a cache miss into a buffer of its own, and a lone put
beside an ordered index over a random int took 1 324 ns, 1 098 now (512 to 256
moved what a write shifts; the reads did not move). `tests/sorted.rs` checks
every filter, order and page against a twin collection without the index. It is
derived data like the hash and text indexes: built when a statement first reads
it after an open (below), never in the file.

## Composite indexes

**An index over several fields is its parts' bytes laid end to end**
(`composite.rs`, `Schema::composites`). `@sorted(owner, created desc)` among
a `create collection`'s fields or `create index on posts (owner, created
desc) @sorted`, `@unique(org, slug)` the same refusing a second holder of
every part, `drop index on posts (owner, created)` by its fields; 2 to 8
parts of bool, int, float, timestamp, text or json, a path among them. The
schema writes them one nameless place past its last behind type tag 16
(`TAG_COMPOSITES`), which a binary from before refuses. A part's value is
encoded so its bytes order as `cmp_value` orders the values -- an int's sign
bit flipped, a float's bits made to order with `-0.0` folded, a bool a tag,
text escaped (`00` as `00 FF`) and ended (`00 01`), a json number as its
`f64` below json text -- null a tag below every value, a descending part's
bytes inverted; a collated text is kept as written behind a tag naming the
collation, and an index holding one compares part by part (`cmp_keys`),
the rest as bytes. A float's `NaN` and a json bool, list, object or int past
2^53 are apart: their codec bytes are their part (a unique index finds them
by them), the rows are listed, and while one is no walk answers and a
filter's run takes them as candidates to test. Entries are `(KeyBytes,
DocId)` in the ordered index's `Chunked`, a key of up to 22 bytes held in the
32-byte entry (an int and a time are 18), longer on the heap, two held ones
compared as a `u128` and a `u64` big-endian and then by length: each boxed,
a million keys counted 48 MB and a put cost 2.0 us; `memcmp` a comparison
was 13% of an insert. Natively the build sorts the keys once (1 015 -> 148
ms over a million); the browser module inserts them, a sort of these entries
being code of its own there. The planner (`composed`) fixes the parts the
`and` chain's equalities name, in order -- an `in` whose every element it
can say makes a span each, `MAX_SPANS` (256) at most together -- narrows the
part after them by its comparisons, and calls the plan exact when it took
every term and nothing is apart; any term it did not take is tested row by
row, so a literal it cannot say (`null`, `12.5` against an int) changes
nothing. `filter_candidates` takes its spans by exact count, two searches
each, under a range's rule; `walk_composite` an `order` on the parts after
the fixed ones in their directions or every one reversed (an `in`'s part is
no fixed one for an order), the spans merged by the order's parts
(`CompositeIndex::walk`), ties gathered and put in id order, a walk testing
the filter only while its run is no longer than `indexed_size`'s smallest
set and giving up past an eighth of the collection; `count_composed` counts
an exact plan off the chunks. A read names it only by its first part
(`names_first`), so one it cannot help neither builds it nor, pinned, meets
it -- and `pin` declines a read naming a composite's first part. Over a
million posts and 10 000 owners `where owner = $1 order created desc limit
20` took 6.3 us p50 against 11.8 through `owner @hash` and 24.4
ms through `created @sorted`, a feed over ten owners 22 us against
100, built in 147 ms in 40 MB; a lone put 715 ns, 1 561
beside it (its first part puts a write anywhere in it), 851 beside
`created @sorted` and `owner @hash`; `@unique(org, slug)` 1 745 ns a put
against 1 285 for `@unique` on the two joined, a read by both 2.5 us
against 1.3 -- a hash index over several fields would answer that by a
microsecond sooner, not worth a third index's code (`make composite-bench`).
`create index` builds it beside the database (`begin_composite` copies the
keys under the read lock); a block puts one made or dropped back
(`Undo::Composed`, `Uncomposed`); a field renamed renames its parts, one
dropped takes it off, either way unbuilt for the first read
(`fit_composites`). A replica builds what the schema declares, a sync
replica a unique one as an ordered one (`schema_ddl`, `schemaDDL`, from
`/collections`' `indexes`). Without `sorted` a file declaring one opens, its
queries the scan's and a unique one asked by reading the rows
(`holds_tuple`). `tests/composite.rs` holds every filter, order and page --
`limit $1` too -- to a twin collection without it through writes, a block
put back, a reopen, an index made later and one beside the database, a
rename and a drop, NaN, nulls, `-0.0`, collations and json paths. The
browser module grew 43.6 KB, 14.1 KB brotli -- the index, its planner, its DDL
and its declared schema, no one function past 4 KB -- and the test build
without the indexes 12.8 KB, 3.9 KB brotli.

## Lazy indexes

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
cost the module 0.7 KB brotli, most of it a builder and a cell a kind. Nor
does an open list a collection's ids where nothing is built from its
documents (`rebuild_indexes_with`): listed for every collection, they were
2.1 ms of a million rows' open.

## warm

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

## @geo

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
(`DataType::keeps_numbers`, `geo::is_geo_call` in `exactly_for`'s
walk), as a json
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
