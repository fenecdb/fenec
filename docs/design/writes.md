# Writes

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *Writes*: `insert` and `put`, arithmetic in `set`, `require`, upserts, picked writes, an `order` by expression and `returning`. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## insert and put

**`insert` never writes over; `put` does.** `Statement::Put` carries
`insert` (the parser sets it for the keyword, `POST /<name>` over REST):
a document naming an id the store holds is `Error::Duplicate` -- 409 over
HTTP, as `Exists` is for a collection -- and the statement, a block of
one, is put back whole. JWT scoping keeps
the flag as it rewrites the statement; made a `put` there, a scoped insert
would write over. The JS builder's `.insert()` still sends `put`: the sync
layer writes rows back through it when it undoes an optimistic write.

## Arithmetic in set

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

## require

**`require <n>` makes a write's count a condition.** A write that matched
no row answered `affected 0` and its `/batch` went on: a transfer's debit
that found too little money was passed over and the credit made money,
2 400 transfers taking the sum from 200 000 to 411 409. `put`, `insert`,
`set` and `del` carry `require: Option<u64>` (the parser's `require` after
the statement, an integer and never a parameter: the count is what the
statement guards, a constant of the code that never makes a text of its
own, where a page's `limit` is a request's data and takes one, below),
and `execute_inner` hands the answer to `required`, which refuses
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

## Upserts

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

## Picked writes

**A `set` or a `del` picks its rows with `order` and `limit`, and
`returning` answers them.** A job queue's claim was a read of the ready
page and a compare-and-set of it, and two workers that read the same page
raced, one of them told only by an `affected` short of its page. `Update`
and `Delete` carry `pick: Option<Box<Pick>>` (boxed: most writes have
none), and `picked_ids` hands the filter, `order` and `limit` to a `get`'s
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
`returning` holds what it answers to its read rules as the write leaves
each row (`Scope::returning`, `Hook::answering`, above), as PostgreSQL
holds `RETURNING` to the `SELECT` policies; `WITH CHECK`, `update(..)`
grants and transitions judge each row as any `set`'s. A sync replica refuses one on a
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

## Order by expression

**A key of `order` may be an expression** (`Sort::expr`), in a `get` and
in a write that picks its rows: `set orders {courier: $1} where run_at <=
now() and distance(pickup, $2) <= 3000 order distance(pickup, $2) limit 1
returning *` is a courier's claim of the nearest ready order, which the
dispatch example wrote in three steps (a read of the six nearest with
`near`, a `/batch` of a conditional `set` a candidate, a held claim of
the oldest) while `order` took fields alone. `order_list` parses an
expression: a field, or an aggregate of one, stays a name -- over groups a
key names a column -- and anything else is `Sort::expr`, `field` empty.
`page_ids` hands a key list holding one to `order_ids_by` (out of line,
so an order of fields keeps its path): the computed columns' `Reader`
decodes every field and path the keys read in one pass a row, a field key
keeps its collation and fold, an expression's text orders by its bytes
or the `collate` named, and ties go in id order, as the scan leaves them
(`tests/order_expr.rs` holds 7 keys x 4 filters x 5 pages x both ways to
the rows read with the key as a column and sorted in the test).
`distance(<field>, <point>)` ascending with a `limit` over a field with
`@geo` walks the point index to the page (`walk_distance`, through
`walk_geo_near`) where no row the filter passes can lack a point -- the
index holds every row, or the filter's `and` chain puts a radius or a box
on the field -- since the scan puts a null distance first and the index
holds no such row; natively only, the browser module sorting the same
rows, where the walk was 1.9 KB of it, 0.9 KB brotli. A pin declines it,
`exactly` walks a key's points, and `check_order` refuses an inner `get`,
an aggregate and `expired()` in a key (a `get`'s `check`, a pick's
`page_of`), an expression over groups (named with `as` first) and one in
a `lookup` level. A write takes no `near`: the parser says to write the
distance as a key. Over HTTP, 300 000 ready orders over a 20 km city
(`make nearest-bench`): one claimer 0.059 ms p50 against the three
steps' 0.135, 16 at once 0.61 ms and 19.3-20.5k claims/s against 1.06-1.10
and 12.9-13.6k, 50 at once 2.24-2.42 ms p50 and 12.5-13.3 p99 against
3.09-3.86 and 14.3-21.2, 15.2-15.8k/s against 10.7-10.9k; the inner
`get`'s `near` (`set ... where id in (get ... near pickup $2 limit 1)`)
answers as the `order` does. 16 threads claiming 3 000 orders one at a
time each took the nearest still ready, none twice, both ways. An order
of fields alone measured as before in process, in ABBA turns on a quiet
machine: a scan of 50 000 matches put in order 6.70 and 6.68 ms against
6.78 and 6.68, a pick of one 5.54 against 5.63 and 5.53, a lone put and
a read by id inside their spread; `make wasm-speed` the same or a little
quicker in every column. With a `put`'s `returning` (below), the exact
walk and a clash's stored value, the browser module grew 4.4 KB, 1.9 KB
gzip, 1.5 KB brotli -- `order_ids_by` 1.3 KB of it, the parser's clauses
0.4 -- after the distance walk and the refusals' long texts came out.

## put returning

**A `put` answers the rows it wrote with `returning`.** An app inserting
an order read it back by a key of its own in the same `/batch` to learn
its id (`insert ... returning id` was `unknown command returning`, `POST
/<name>` `{"inserted": n}`). `Statement::Put` carries a `pick` too, its
`returning` alone (`order` and `limit` are a parse error after a `put`),
so `Statement::pick` and `returns_rows` take it as they take a `set`'s;
`put_with` gathers each written row's id and the write it was -- an
insert, an update of a row written over or an upsert's `else set` -- only
when asked, and `returned` reads them back as the statement left them, in
the documents' order: a row `if absent` passed over is not one, a key
twice is its row twice, and `require` counts them. Each row goes to
`Hook::answering` as the write it was (`returned` takes one op for every
row or one a row), so a scoped token's insert is held to its read rules
as written and refused whole (403) where one falls outside them, with no
rule of `access.rs` of its own. `POST /<name>?returning=id,...` answers
the rows (201). A sync replica refuses one on a synced collection as it
refuses a claim (`PICKED`, a scenario step): its ids are the replica's
until the server answers. Every builder takes it (`insert(docs, {
returning })`, Python's `returning=`, Go's `Returning` among the
documents, .NET's `returning:`, Swift's, Kotlin's and Dart's
`insertReturning`/`upsertReturning`), held to the golden file.
