# fenecdb

[![ci](https://github.com/fenecdb/fenec/actions/workflows/ci.yml/badge.svg)](https://github.com/fenecdb/fenec/actions/workflows/ci.yml)

Minimal, vector-native, browser-resident embedded database. Written in Rust,
compiles to WebAssembly, has its own query language (**FenecQL**) and speaks the
PostgreSQL protocol.

```
create collection articles (
  title     text,
  tags      [text],
  year      int   @hash,
  embed     vector<768> @hnsw(cosine, m=16, ef_construction=200)
)

get articles select title
  where year >= 2024 and tags has "rust"
  near embed $1
  limit 10
```

The same query from JS — no ORM, no npm, no build step:

```js
const rows = await db.from('articles')
  .select('title')
  .where('year', '>=', 2024)
  .where('tags', 'has', 'rust')
  .near('embed', queryVector)
  .limit(10)
  .rows();
```

---

## Design decisions

### 1. No page cache (buffer pool)

Classic databases split the disk into pages and copy those pages into a cache
in user space. The eviction policy, dirty-page tracking, checkpoints and the
locking cost all come from there.

fenecdb skips that layer entirely:

- Segments are **immutable**. A write only appends to the active segment.
- The byte sequence on disk and the byte sequence in memory are **the same
  format**. So there is no "caching" stage — a read is decoded directly over
  the arena slice.
- The only helper structure is the `HashMap<DocId, Loc>` offset index.
- Projection and filters decode a single field with [`Store::read_field`]; the
  fields in front of it are skipped without allocating.

The result: no eviction policy, no dirty pages, no warm-up time. Dead bytes are
reclaimed with `compact`.

### 2. Vectors are first-class citizens

`vector<N>` is a **type**, `@hnsw` is an **index**, `near` is a **query
clause**. Not a plugin — the language itself:

```
get articles where year >= 2024 near embed $1 ef 200 limit 10
get articles near embed $1 exact limit 10         -- exact scan (for verification)
```

- HNSW is written in-house; every vector lives in one contiguous arena — `f32`
  for `vector<N>`, half the size for `vector<N, f16>` (below).
- With the cosine metric vectors are normalised inside the index → the query
  is only a dot product.
- When a filter and a vector are used together the **filter set comes first**,
  then one of two paths is taken:
  - if the set is smaller than the candidate count an ANN walk would measure
    anyway (`ef × m0`), that set is scanned directly — cheaper and exact;
  - otherwise the ANN runs and the candidates go through a membership test; if
    the result stays under the limit the filter set is scanned in full.

  The fallback in the second step is required: the membership test is applied
  *after* the candidates are gathered, so when the filter field correlates with
  the vector every one of the `ef` nearest neighbours can be eliminated. The
  query would then come back empty although thousands of documents match.
- Neighbour selection uses the diversity heuristic (Malkov & Yashunin, Alg. 4);
  taking only the M nearest candidates trapped the graph in local clusters and
  lowered recall.
- The read-only part of the build (descent + beam search + neighbour selection)
  runs **in parallel** over a batch; only writing the links is serial. The
  result is independent of the thread count and **deterministic**.
- The HNSW graph is written to the file — **the links, not the vectors**. The
  vectors are already in the document records; the expensive part is the graph
  build. On open it is validated and silently rebuilt if stale (derived data).

#### Half precision: `vector<N, f16>`

Precision is part of the **type**, not of the index — like pgvector's `halfvec`.
This choice shrinks the record and the arena together; as an index option only
the arena would shrink, yet in the browser the file image is in memory too.

```
create collection docs (embed vector<768, f16> @hnsw(cosine))
```

The runtime representation is unchanged: values are `f32` everywhere, conversion
happens only at the storage boundary. 100 000 × 128, clustered, same data:

| | `vector<128>` | `vector<128, f16>` |
|---|---|---|
| vector arena | 51.2 MB | **25.6 MB** |
| file image | 57.9 MB | **32.3 MB** |
| index build | **10.4 s** | 12.5 s |
| ANN p50 | **0.127 ms** | 0.147 ms |
| recall@10 | **100%** | 99.6% |

The cost is in the arithmetic: distance over an f16 arena is computed with the
unpacking inside the loop. Two things were measured and fixed:

- **The expansion has to be branchless.** The first version called `codec`'s
  exact conversion; its subnormal branch killed auto-vectorisation and made the
  build 5 times slower (52 s). The branchless version is a single multiply and
  is bit-identical to the exact conversion over all 65 536 values (tested).
- **The fixed side should be unpacked once.** Most of the build time goes into
  the diversity loop of neighbour selection: every candidate is compared with
  all of the already selected ones (~`ef × m` distances) and each comparison
  unpacked both vectors. The selected set is fixed throughout the loop, so it
  is unpacked once and kept; 22.6 s → 12.4 s.

The remaining gap is the cost of unpacking itself: 1.2× on build, 1.16× on
query. The query side staying this cheap is no accident — the bytes read per
vector are halved, so part of the unpacking cost is won back from memory
traffic.

The win is at the memory boundary: 1 M × 768 vectors take ~3 GB with f32,
~1.5 GB with f16. In WASM32's 4 GB address space that means a data set that
did not fit now fits. The output of embedding models sits comfortably inside
the f16 range (±65504, ~3 decimal digits); measured recall loss is 0.3 points.

### 3. Zero dependencies

The core uses only `std`. Its own binary codec, its own JSON, its own HNSW,
its own lexer/parser. The reason: keeping the WASM output small and
auditable.

```
$ wc -c < web/fenec.wasm
308787   # the whole engine: storage + HNSW + FenecQL + JSON
```

The browser has a single thread, so the parallel build path is cut out of the
target entirely with `cfg` — no thread code enters the WASM output.

### 4. Straight into the browser, without wasm-bindgen

`wasm-bindgen` requires an npm chain and `wasm-pack`. Everything fenecdb needs in
the browser is "take text, return text", so the exported surface is a plain C
ABI:

```js
import { Fenec } from './fenec.js';

const db = await Fenec.open('./fenec.wasm');      // one fetch, no glue
db.run('create collection docs (title text, embed vector<384> @hnsw(cosine))');
db.run('put docs {title: "hello", embed: $1}', [embedding]);
const { rows } = db.run('get docs near embed $1 limit 5', [query]);
```

The JS client is `web/fenec.js` — no dependencies, no build step. The WASM glue
is ~165 lines of it; the rest is the query builder, HTTP client and sync layer.
For persistence, `persist(db)` / `restore(db)` write the snapshot to IndexedDB.

### 5. PostgreSQL plugin

`fenec-pg` is a server that speaks the PostgreSQL v3 wire protocol. Any
PostgreSQL client — psql, psycopg, node-postgres, JDBC, pgbouncer — can
connect to fenecdb and run FenecQL:

```
$ fenec-pg --listen 127.0.0.1:5433 --file data.fenec
$ psql -h 127.0.0.1 -p 5433 -U fenec
fenec=# get articles select title near embed '[0.1, 0.2]' limit 5;
```

What is compatible is the **transport layer, not the language**. fenecdb does
not speak SQL; it does work with the connection tooling of the PostgreSQL
ecosystem. Vectors use the same text representation as pgvector (`[1,2,3]`),
errors map to PostgreSQL SQLSTATE codes (`42P01`, `42601`, `42804`, `57014`).

**Session model.** One thread per connection. Read-only statements run *at the
same time* under a shared lock (`Database::query`), writes take the exclusive
lock (`execute_with`). HNSW search buffers are kept thread-locally, so the same
index can be searched in parallel — the measured speed-up for 8 concurrent
full scans on 8 cores is 4.3×.

**Resource ceilings.** One thread per connection means that, with unlimited
connections, the ceiling on stack memory is left to the client.
`--max-connections` (default 100) rejects the connection above it with `53300`
— PostgreSQL's *sorry, too many clients already*. `--idle-timeout <s>` closes a
session that has gone quiet with `57P05` (off by default). `--max-message
<MiB>` (default 64) is the ceiling of a single protocol message; the length is
read before the body, so the body of an oversized message is never allocated at
all. The startup packet is limited to 10 000 bytes on top of that (the same as
PostgreSQL) — that allocation happens *before* authentication.

`--max-memory <MiB>` puts a ceiling on the data footprint (off by default).
Above it, statements that **grow** the data are rejected with `53200`; reads,
`del` and `compact` keep working — hitting the ceiling with no way out would be
no better than a cgroup OOM. The point is to bring the OOM forward: nobody
warns you about a process felled by `SIGKILL`, and both the last `sync` and the
checkpoint are lost. The measurement happens *before* the statement, so the
overshoot is at most one statement, whose body `--max-message` bounds as well.

**Health check.** `fenec-pg --ping` connects, authenticates and exits; `0` means
up. It is the same depth as `pg_isready` and deliberately runs no query: every
query takes the database lock first, so during a long `compact` the probe would
wait too and a healthy server would look dead.

**Durability.** Writes accumulate in a 1 MB buffer; `--sync` decides when they
reach the disk:

| `--sync` | meaning | cost |
|---|---|---|
| `always` | `fsync` after every write statement | ~3 ms/write |
| `250` (default) | periodic; at most one interval is at risk | negligible |
| `off` | on shutdown only | none |

`SIGINT`/`SIGTERM`/`SIGHUP` are caught and a final `sync` runs before exit.
The exclusive lock is **held** until exit: no new write can be accepted between
`sync` and `exit`, so no write is left that looked successful to the client but
never reached the disk. Sessions waiting on the lock do not wait for nothing,
they come back with `57P01`. Under `kill -9` you get exactly as much as the
chosen policy promises.

After that, if a vector index exists, a **checkpoint** is written: the HNSW
graph lands in the file and the next open does not rebuild it — 110 ms instead
of 10.2 s at 100 000 × 128. The cost is that the whole image is produced in
memory right then (peak ≈ 3× the file) and that shutdown gets longer on a large
database; `--no-checkpoint` turns it off. The order is deliberate: `sync` comes
first, so even if the process is killed while the checkpoint is being written
the data is already on disk and only the graph is lost — `rewrite` writes to a
side file and `rename`s it, so a half-written checkpoint cannot corrupt it.

**Query cancellation.** `CancelRequest` is a real cancellation: the pending
lock is released and the query returns `57014` (measured latency ~1 ms), the
connection stays usable. A cancel that arrives while idle, or one with the
wrong secret key, is ignored.

**Both directions.** The same protocol code is used the other way round: `fenec
import` connects to a real PostgreSQL server and streams `COPY ... TO STDOUT`
(see *Import*). The server half of SCRAM is in `scram.rs`, the client half in
`client.rs`; both are hand-written, so they test each other.

**Authentication.** With `--password`, the default is **SCRAM-SHA-256**
(RFC 7677): the password never goes over the wire and the exchange cannot be
replayed. `--auth cleartext` is there for clients that cannot speak it.
`--user` restricts the user name. The password can also be given with
`--password-file` or `FENECPG_PASSWORD` (`argv` shows up in `ps` output).

**There is no TLS.** `SSLRequest` is refused with `N`; a client connecting
remotely gets a `NoticeResponse` telling it the connection is unencrypted.
Listening on a non-loopback address without authentication is refused (it can
be opened deliberately with `--insecure`). For use on an open network, put it
behind a TLS terminator such as stunnel or `nginx stream`.

**Extended protocol.** `Describe` is answered without running the query: the
parameter count comes from the largest `$n` in the statement, the columns and
type OIDs from the schema (with `near`, `_score` is reported too). Parameter
types are left `unspecified`: the client sends text, the server infers.
`RowDescription` goes exactly once — on `Execute` when `Describe` is skipped.

The plugin system takes after PostgreSQL's extension model: scalar
functions, write hooks (trigger-like) and transport adapters can be
registered.

```rust
db.install_plugin(&PgPlugin)?;   // pg_version(), pg_typeof(), to_pgvector()
```

### 6. The replica is an endpoint, not a library

Because the same engine runs in the browser and on the server, the "local copy
+ server" architecture needed no extra layer, only two things: the server being
able to say "these changed", and the client being able to choose where to bind
a query. The second already existed (`bind()`), and the first was sitting in
the file format — the file is already a change log.

```js
const db = await sync({ url, shapes: [{ collection: 'tasks', key: 'key' }] });
await db.from('tasks').where('priority', '>=', 3).rows();   // local, no network
```

It is TanStack DB's model, with two differences. **No incremental dataflow**:
there a collection is a `Map` and running the filter on every keystroke over a
large set is unacceptable; here the local side is indexed. **A shape is
mandatory**: the whole database is in memory; the client cannot pull all of it.

Details: [Sync](#sync-local-replica-and-server).

---

## Compared with SQLite and PostgreSQL

Measured with `crates/fenec-bench`: **same data, same process, same distance
kernel**. Apple M-series, 100 000 rows × 128 dims, clustered embeddings.

```bash
make compare          # without PostgreSQL that arm is skipped
```

| | fenecdb | SQLite 3.46 | PostgreSQL 17 + pgvector 0.8.6 |
|---|---|---|---|
| data write (no index) | **900 k rows/s** | 415 k/s | 64 k/s |
| index build | **10.2 s** (HNSW) | 0.03 s (B-tree only) | 14.2 s (HNSW) |
| total | 10.3 s | **0.27 s** (no ANN) | 15.7 s |
| disk size | **58.4 MB** | 59.8 MB | 145.0 MB |
| scalar filter (indexed) | **4.3 ms** | 14.5 ms | 7.8 ms |
| vector top-10, **exact** | **4.0 ms** | 31.3 ms | 14.8 ms |
| vector top-10, **ANN** | **0.18 ms** | none | 0.91 ms |
| → recall@10 | **100%** | — | **100%** |
| reopen | 117 ms | **1.0 ms** | server (always open) |
| empty query round trip | 0 (embedded) | 0 (embedded) | 0.49 ms |

All three engines follow the same workflow: **bulk load first, index after**.
That is already the recommended order for SQLite and PostgreSQL; in fenecdb
`create index` exists for exactly this.

### Method

- SQLite: WAL + `synchronous=NORMAL`, bulk write in one transaction, index
  built afterwards (the recommended order). fenecdb does not `fsync` per write.
- PostgreSQL: in Docker, bulk load with COPY, then
  `hnsw (m=16, ef_construction=200)` — the **same** parameters as fenecdb.
  At query time `hnsw.ef_search = 64`, again the same as fenecdb's default.
- Vector distance is computed with the same code in all three engines; what is
  measured is the engine's cost of *fetching the data*, not the arithmetic.
- SQLite's core has no ANN index; a vector search is a full scan. So first
  **exact against exact** (equal semantics), then ANN on a separate row.
- The write is split in two stages: writing the rows and building the index.
  A single "total" number would weigh an engine that builds an ANN index and
  one that does not on the same scale.
- The PostgreSQL numbers include a 0.40 ms TCP round trip — ~40% of the ANN
  time. That is not a measurement flaw, it is the client-server architecture.

### How to read the numbers

**fenecdb leads on raw write speed**: 900 k rows/s, more than twice SQLite's.
The append-only segment store reduces a write to appending to an arena — no
B-tree rebalancing, no page splits.

**It leads on index build too**: 10.2 s against pgvector's 14.2 s, with the
same `m` and `ef_construction`. SQLite's 0.03 s is not an ANN index but a
single-column B-tree; that cost does not vanish, it is deferred to query time
— a vector search takes 31.3 ms, **171× slower** than fenecdb's ANN.

**The one real loss is on reopen**: SQLite opens in 1 ms because it loads
nothing, reading pages as it needs them. fenecdb pulls the vector arena into
memory and validates the graph — 117 ms. That is not a shortcoming but the
other side of the same coin: it is why queries are 3–171× faster. The measured
break-even is **4 vector queries**; after that fenecdb is ahead on the total.

**fenecdb leads on disk as well** — 2.5× smaller than pgvector. PostgreSQL's
extra is MVCC row headers, WAL and the visibility map: the price paid for
concurrent transactions. fenecdb's single-writer model does not need it.

### Architectural differences

| | fenecdb | SQLite | PostgreSQL |
|---|---|---|---|
| deployment | embedded + **browser (WASM)** | embedded | server |
| query language | FenecQL | SQL | SQL |
| vector | **in the language core** | none (needs a plugin) | `pgvector` extension |
| page cache | **none** | page cache | shared buffers + MVCC |
| concurrency | single writer | single writer (many readers via WAL) | many writers, MVCC |
| transactions | none | ACID | ACID |
| JOIN / subquery | none | yes | yes |
| runtime size | 302 KB wasm · 636–863 KB binary | ~1 MB | ~30 MB + server |
| dependencies | **zero** | zero | many |

### Which one, when

**fenecdb** makes sense when vector search has to be first-class and when the
database should run on the user's machine — especially in the browser: local
semantic search, offline RAG, in-browser agent memory, embedded
recommendation. The scale limit is **the file image fitting in memory**: not
just the vector arena, all of the records are in memory (see [Limits](#limits)).

**SQLite** when you need relational data, transactional safety, JOINs and a
mature toolchain. Also in very short-lived processes: for a tool that runs
fewer than five queries and exits, fenecdb's open cost never amortises.

**PostgreSQL + pgvector** for multi-writer systems shared over a network that
need ACID. On maturity, concurrency and ecosystem the comparison is not even
worth making — fenecdb is not aiming at that job. That is exactly why `fenec-pg`
exists: not to *replace* PostgreSQL but to reach fenecdb with the same tools.

---

## FenecQL

```text
create collection [if not exists] <name> ( <field> <type> [@index], ... )
drop collection [if exists] <name>
create index [if not exists] on <name> (<field>) @index

put <name> { field: value, ... }              -- or [ {...}, {...} ]
get <name> [select a, b] [where <expr>]
           [near <field> <vector> [ef N] [exact]]
           [order <field> [asc|desc], ...] [limit N] [offset N]
get <name> [where <expr>] count               -- number of matching rows
select a, b from <name> [where ...]           -- the classic SQL order works too
set <name> { field: value } [where <expr>]
del <name> [where <expr>]

collections | describe <name> | compact [<name>]
```

| | |
|---|---|
| **Types** | `bool` `int` `float` `text` `bytes` `timestamp` `vector<N[, f16]>` `[type]` |
| **Indexes** | `@hash`, `@hnsw(metric, m=.., ef_construction=.., ef_search=..)` |
| **Defaults** | `@hnsw(cosine, m=16, ef_construction=200, ef_search=100)` |
| **Metrics** | `cosine` `l2` `dot` |
| **Operators** | `= != < <= > >=`, `~` (text contains), `has` (list contains), `in [..]`, `is null` |
| **Logic** | `and` `or` `not` — `and` binds tighter than `or` |
| **Parameters** | `$1`, `$2`, … (same as PostgreSQL) |
| **Functions** | `lower upper len coalesce now timestamp cosine l2 dot norm normalize` + plugins |
| **Ordering** | `order year desc, title asc` — when the first key ties the second decides; `id` can be ordered too |
| **Counting** | `count` returns one row with one column; it does not combine with `select`/`near`/`order`/`limit`/`offset` |
| **Limits** | `near` at most 10 000 rows (`limit + offset`), expression depth 512 levels — [Limits](#limits) |

The `id` field is automatic. Giving `id` inside a `put` makes it an upsert.

**Time.** `timestamp` holds UTC epoch milliseconds (i64). On write both text
and numbers are accepted; in a comparison a text literal is parsed:

```
put events {name: "login", t: "2026-09-19T12:34:56Z"}
put events {name: "logout", t: 1758285296000}        -- epoch ms
get events where t >= "2026-01-01" and t < now()
```

The representation is always ISO-8601 (`2026-09-19T12:34:56.789Z`); on the
wire `fenec-pg` reports PostgreSQL's own output format
(`2026-09-19 12:34:56.789+00`) and the `timestamptz` OID, because client
parsers expect the server format. The calendar conversion is integer
arithmetic — leap year and century rules included, no table, no dependency.

It was added as a separate type rather than an alias over `int`: `ResultSet`
does not carry the schema, so an alias would show every client a raw number.

For a bulk load, building the index afterwards is much faster — the write path
becomes a pure append and the graph is built in one pass, in parallel:

```
create collection docs (title text, embed vector<768>)
put docs [ ... 100 000 documents ... ]
create index on docs (embed) @hnsw(cosine)
```

---

## Query builder

FenecQL is readable, but on the application side the job is still **building a
string**. Every interface with conditional filters ends up in the same place:

```js
let q = 'get articles where 1=1';
if (year) q += ` and year >= ${year}`;    // ← quote escaping + `$n` discipline by hand
```

What drives people to an ORM is usually not the ORM itself but these two:
dynamic queries and type safety. The builder inside `web/fenec.js` gives both —
without npm, a build step or a second schema definition.

```js
import { Fenec } from './fenec.js';
const db = await Fenec.open('./fenec.wasm');

const rows = await db.from('articles')
  .select('title', 'year')
  .where('year', '>=', 2024)
  .where('tags', 'has', 'rust')
  .near('embed', queryVector, { ef: 128 })
  .limit(10)
  .rows();
```

The real win is on the dynamic side; every `where` is `and`ed with the previous
one and the query object is **immutable**, so a body can be shared and branched:

```js
let q = db.from('articles').select('title');
if (year) q = q.where('year', '>=', year);
if (tag)  q = q.where('tags', 'has', tag);
const rows = await q.limit(20).rows();
```

### Filters

All three spellings compile to the same thing; whichever reads better:

```js
.where('year', '>=', 2024)                   // field, operator, value
.where('category', 'book')                   // two arguments = equality
.where({ year: { gte: 2024 }, tags: { has: 'rust' } })
```

| | |
|---|---|
| **Operators** | `=` `!=` `<` `<=` `>` `>=` `~` `has` `in` |
| **Word forms** | `eq` `ne` `lt` `lte` `gt` `gte` `like` `has` `in` |
| **`null`** | `{ summary: null }` → `is null`, `{ summary: { not: null } }` → `is not null` |
| **Combining** | `or(...)` `not(...)` `and(...)` — `where` already `and`s |
| **Escape hatch** | `raw('cosine(embed, ?) > ?', vector, 0.5)` |
| **Ordering** | `order('year','desc').order('title')` — successive calls add keys |
| **Endpoints** | `rows()` `first()` `count()` `run()` `toFenecQL()` |
| **Writes** | `insert(doc\|docs)` `update(object)` `delete()` |

`or` and `not` are separate functions because the `where` chain is an `and`; a
nested group is written explicitly and the parenthesising is left to the builder:

```js
db.from('articles')
  .where('year', '>=', 2024)
  .where(or({ tags: { has: 'rust' } }, { title: { like: 'rust' } }))
// get articles where year >= $1 and (tags has $2 or title ~ $3)
```

`raw` is for everything the builder cannot express — in practice, today,
function calls. The `?` placeholders are bound to parameters in order, so the
escape hatch does not turn into string concatenation either.

### Seeing what it produces

The builder does nothing hidden. `toFenecQL()` hands back exactly the text and
parameters that will run — loggable, testable, runnable by hand:

```js
db.from('articles').where('year', '>=', 2024).limit(10).toFenecQL()
// ['get articles where year >= $1 limit 10', [2024]]
```

Every leaf value is bound to a parameter; text is never embedded into the
query. That leaves a single injection boundary: **names**. Collection and
field names cannot be parameterised, so they are validated against FenecQL's
identifier rule and a name that does not fit errors out (like `'a or 1=1'`).

```js
db.from('articles').where('title', '"; del articles; --')
// get articles where title = $1     ← a value, not text
```

### Independent of the transport

The builder does not have to know the connection: `from()` builds a query on
its own and `bind()` plugs it into an executor. The same query code runs
unchanged in wasm, over `fenec-pg`, or on an HTTP endpoint later on.

```js
import { from, connect } from './fenec.js';

const q = from('articles').where('year', '>=', 2024);
await q.bind(db).rows();                            // wasm, local
await q.bind(connect('http://localhost:8080')).rows();  // remote server
q.toFenecQL();                                        // or just the text
```

For the remote endpoint: [HTTP endpoint](#http-endpoint). `connect()` returns
the same builder; the FenecQL it produces goes to `POST /query` as is.

A standalone `from()` cannot know the collection name — the schema is not in
`db` but in the call. In TypeScript it binds to the schema through `TypedFrom`;
at runtime it is the same function, the only difference is name completion:

```ts
import { from as fenecFrom, type TypedFrom } from './fenec.js';
import type { FenecSchema } from './fenec-schema.js';

const from: TypedFrom<FenecSchema> = fenecFrom;
from('articles').where('year', '>=', 2024);   // completes like db.from
```

This is why the builder's endpoints return a `Promise` while `db.run()` stays
synchronous: raw FenecQL is one wasm call, the builder must pick its transport.

### Types

The schema is not defined a second time in TypeScript — it is generated from
the live file, so the two cannot drift apart:

```bash
fenec types data.fenec > web/fenec-schema.d.ts     # or: make types FILE=data.fenec
```

```ts
import { Fenec } from './fenec.js';
import type { FenecSchema } from './fenec-schema.js';

const db = await Fenec.open<FenecSchema>('./fenec.wasm');

const rows = await db.from('articles').select('title', 'year').rows();
//    rows: { title: string; year: number | null }[]
```

The declaration carries the schema line in a comment and reflects this mapping:

| schema | TypeScript |
|---|---|
| `bool` `int` `float` `text` | `boolean` `number` `number` `string` |
| `timestamp` | `Timestamp` — ISO text on read; a `Date`/number works on write |
| `vector<N>` / `bytes` | `Vector` / `Bytes` — `number[]` on read, `Float32Array` on write too |
| `[type]` | `type[]` |
| a field that is not `required` | `\| null` — an unwritten field reads as `null` |

What is caught: a collection or field that does not exist, a wrong value type,
access to an unselected field, `near` applied to a non-vector field, a type
mismatch inside `insert`. Objects inside `or`/`and`/`not` are checked against
the query's collection too — not against the first argument. The result of a
query that uses `near` gains `_score: number`.

`fenec.d.ts` is hand-written (the client's own surface), `fenec-schema.d.ts` is
generated. There is no link between them: the `Timestamp`/`Vector`/`Bytes`
brands are structural, so the generated file stands alone and needs no import.

### Limits

- **No aggregation other than `count`.** `count()` compiles to the language's
  own `count` clause (rows are not decoded), but there is no
  `sum`/`avg`/`group by` — the builder cannot invent what FenecQL does not have.
- **An `update`/`delete` without a filter is refused.** Covering the whole
  collection by accident is far too easy and impossible to undo; when it is
  deliberate you write `delete({ all: true })`. (PostgREST decides the same.)
- **`near`/`order`/`limit` error out on write statements** — FenecQL does not
  support them on `set`/`del`, and ignoring them silently would breed the
  illusion that "`limit(1)` deletes a single row".
- **No JOIN, no subquery, no `returning`** — the builder adds no capability on
  top of FenecQL, it only builds it safely.

The tests run with `node --test web/fenec.test.js` (no dependencies, part of
`make test`); when `web/fenec.wasm` is present, an end-to-end test that actually
parses the generated text runs too.

---

## HTTP endpoint

For clients that want no driver: `fetch`, `curl` or any language's standard
library is enough. The schema is already in the database, so the surface is
derived from it too — PostgREST's `?field=op.value` pattern.

```bash
fenec-pg --file data.fenec --http 127.0.0.1:8080
```

```bash
curl 'http://127.0.0.1:8080/articles?select=title,year&year=gte.2024&tags=has.rust&order=year.desc&limit=10'
# [{"title":"the rust book","year":2024}]

curl 'http://127.0.0.1:8080/articles?year=gte.2024&count'
# {"count":1}

curl -X POST http://127.0.0.1:8080/articles -d '{"title":"new","year":2025}'
# {"inserted":1}
```

**Not a separate binary.** The HTTP endpoint is a second listener in the same
process as `fenec-pg`. The reason is architectural: fenecdb has a single writer
and if two processes write one file the file is corrupted ([Limits](#limits)).
Being in the same process keeps the sync policy, checkpoint, memory ceiling
and shutdown signal in one place — `--sync always` covers HTTP writes as well.

```
GET    /                        version + collection list
GET    /collections             schemas
GET    /<name>?<filter>         rows
GET    /<name>?<filter>&count   number of matching rows
POST   /<name>                  body: {...} or [{...}]
PATCH  /<name>?<filter>         body: {...}
DELETE /<name>?<filter>
PATCH  /<name>/all              no filter — deliberately
DELETE /<name>/all
POST   /<name>/near             body: {"vector":[...], "limit":10}
GET    /<name>/changes?since=N  subscription (SSE): shape + incremental diff
POST   /query                   body: {"query":"<FenecQL>","params":[...]}
POST   /batch                   one query body per line (NDJSON)
```

### Filters

| | |
|---|---|
| **Operators** | `eq` `neq` `lt` `lte` `gt` `gte` `like` `has` `in` `is` |
| **Negation** | the `not.` prefix — `?summary=not.is.null`, `?year=not.eq.1999` |
| **Shorthand** | if the prefix is not recognised the whole value is an equality: `?year=2024` |
| **List** | `?year=in.(1999,2023)` |
| **Clauses** | `select` `order` `limit` `offset` `count` `where` |
| **Ordering** | `?order=year.desc,title.asc` |
| **Free expression** | `?where=year >= 2024 and tags has "rust"` — full FenecQL |

Values are parsed by the field's type (`?year=gte.2024` int,
`?published=gte.2024-01-01` timestamp); an unknown field gives a 404. `where=`
is for what does not fit the pattern: `or` groups, function calls.

### Why is `near` a POST?

A 768-dimensional embedding does not fit in a query string. Trying to squeeze
it into the URL (base64, truncation) is both unreadable and runs into proxy
and server URL ceilings. PostgREST's model does not cover fenecdb's headline
feature, so `near` is a separate endpoint — and it takes a filter in the body:

```bash
curl -X POST http://127.0.0.1:8080/articles/near -d '{
  "vector": [0.1, 0.2, 0.3],
  "ef": 128, "limit": 5,
  "select": ["title"],
  "where": "year >= 2024"
}'
# [{"title":"the rust book","_score":0.97}]
```

If the collection has a single vector field there is no need to write `field`.
Filters in the query string are `and`ed with the `where` in the body.

### Raw FenecQL

The REST surface derives from the schema and is deliberately narrow: no DDL,
no `or` groups. `POST /query` removes that wall — and it is exactly the remote
transport of the [query builder](#query-builder):

```js
import { connect } from './fenec.js';
const db = connect('http://127.0.0.1:8080', { token: 'secret' });

// the same code as in wasm
const rows = await db.from('articles')
  .where('year', '>=', 2024)
  .near('embed', queryVector, { ef: 128 })
  .limit(10)
  .rows();
```

The builder was producing FenecQL text; `POST /query` takes it as is. The query
code does not change between the two transports — the only difference is
`connect()` instead of `Fenec.open()`.

### Security

| | |
|---|---|
| `--http-token <value>` | every request needs `Authorization: Bearer <value>` (constant-time comparison) |
| `--http-cors <origin>` | `Access-Control-Allow-Origin`; without it no CORS header is sent at all |
| `--http-read-only` | write endpoints return 403 — `fenec-pg`'s own path is unaffected |
| non-loopback | without a token it refuses to bind (bypassed with `--insecure`) |

There is no TLS, same rule as `fenec-pg`: on an open network it needs a TLS
terminator in front. A `PATCH`/`DELETE` without a filter is refused; covering
the whole collection needs a separate **path** (`/<name>/all`). It is a
separate path because a key such as `?all=true` would collide with a field `all`.

Two details when opening it in a container: the port has to be published and,
because `CMD` is overridden, **every default has to be written out by hand** —
otherwise `--file` drops and the container comes up with an in-memory database:

```bash
docker run -d --name fenecdb -v fenecdata:/data \
  -p 127.0.0.1:5433:5433 -p 127.0.0.1:8080:8080 \
  -e FENECPG_PASSWORD=secret fenecdb \
  --listen 0.0.0.0:5433 --file /data/data.fenec --sync 250 \
  --http 0.0.0.0:8080 --http-token a-token
```

`0.0.0.0` is non-loopback, so `--http-token` is mandatory — the same rule as
for `--listen`.

The limits are shared with `fenec-pg` (`--max-connections`, `--idle-timeout`);
the body ceiling is 64 MiB, the header block 64 KiB. `Transfer-Encoding:
chunked` is unsupported, refused with 411 — better than reading half a body.

A field whose name matches a clause name (`select`, `order`, `limit`,
`offset`, `count`, `where`) cannot be filtered over HTTP; the FenecQL and
`fenec-pg` paths are unaffected.

---

## Sync: local replica and server

TanStack DB's model — a copy on the client, changes streaming from the server,
optimistic writes — is not a library in fenecdb but an **endpoint** and a
**transport**. Two pieces were ready: the file format is already a change log,
and `bind()` already separated the query from the transport.

```js
import { sync } from './fenec.js';

const db = await sync({
  url: 'http://127.0.0.1:8080',
  shapes: [{ collection: 'tasks', where: { status: 'open' }, key: 'key' }],
});
await db.ready();

// read: from the local copy, no network
const rows = await db.from('tasks').where('priority', '>=', 3).rows();

// live query: re-runs on every change
const stop = db.live(db.from('tasks').order('priority', 'desc'), draw);

// write: to the local copy first (instantly), then to the server
await db.from('tasks').insert({ title: 'new', status: 'open' });
```

The query code does not change: `db.from(...)` is the same builder, the same
FenecQL. The only thing that changes is where it is bound.

### The stream carries state, not transactions

The classic route is a transaction log: every write is an event, the subscriber
applies them in order. fenecdb does **not**, because the cost comes from two
places. The event has to carry the document — a ring buffer carrying a 768-dim
embedding becomes hundreds of MB in a few thousand writes. And ten writes to
the same document in a row are ten events, none of whose interim states help.

The stream here is **state-based**. The ring holds only `(seq, collection,
id)` — 24 bytes per entry, independent of document size. When the subscriber
reads, the row's *current* state is decoded from the store:

```
event: seed
data: {"seq":42,"rows":[{...}]}

event: change
data: {"seq":43,"puts":[{...}],"dels":[7],"schema":false}
```

It has three consequences:

1. **N writes to the same document collapse into one event.**
2. **Re-applying is harmless.** "This is the current state of this id" gives
   the same result applied twice; reconnecting produces no duplicates.
3. **The shape filter works correctly for free.** If a row is updated out of
   the filter the subscriber gets a "gone" answer — no separate "left" event
   is needed. In a transaction log that would mean keeping the old image too.

The cost is plain: the subscriber cannot see interim states, and `seq` is a
*clock*, not a record. Right for a replica; not for anyone wanting an audit log.

### A shape is a set, not a window

The whole database is in memory and the WASM32 address space is 4 GB: the
client cannot pull an entire collection. A subscription is therefore a
**subset**, and the filter is applied on the server side.

`order`, `limit`, `offset` and `count` are **refused** in a subscription
(400). A subscription like "the latest 100 rows" looks correct but is not:
when a new row arrives the oldest has to drop out, and an incremental diff
cannot say that. An explicit error instead of a silently wrong stream.

The shape condition becomes the `?field=op.value` form, so there is no `or`
group and no function call. The reason is escaping: a free `where=` would turn
into string concatenation — exactly what the builder avoids. Escaping goes to
the transport (`URLSearchParams`), types to the server; no injection surface.

**One collection per shape.** The seed has to say "this is the whole of this
collection", otherwise the cleanup step becomes ambiguous.

### Optimistic writes and reconciliation

A write is three steps and the order matters: **first gather what undoes the
local change**, then apply it locally, then send it to the server. If the server
refuses, the local side rolls back fully — no network needed, the undo is in hand.

Everything up to the first `await` is synchronous. `async` runs a function
body synchronously until its first `await`, so the optimistic row is already
local before the caller awaits the promise. A single microtask in between
would have broken the "visible instantly" promise.

```js
const p = db.from('tasks').insert({ title: 'new', status: 'open' });
await db.from('tasks').where('title', 'new').first();  // ← already here
await p;                                               // server ack
```

The id space belongs to the server and the client cannot know it in advance.
The optimistic row therefore gets a temporary id starting at `2^52` — far away
from the server's range and below `Number.MAX_SAFE_INTEGER`. What matches the
two is a **business key** (`key`): when a row with the same key arrives from
the subscription the temporary one is dropped. The counterpart of TanStack
DB's `txid` round; here the round is a key, because the id comes from the server.

> **An insert into a keyless shape is not applied optimistically.** With
> nothing to reconcile, the row would stay a local duplicate; one round trip of
> latency beats a silent copy. `key` must be `text @hash` (as the UUID decision).

Update and delete need no key: they work by id, and reading the previous state
is enough.

### Live queries: no incremental maintenance

TanStack DB needs differential dataflow (d2ts), because there a collection is
a `Map` and running the filter on every keystroke over 50 thousand rows is
unacceptable. Here the local side is **an indexed database**; running the
query from scratch is already under a millisecond.

On top of that an incremental diff would not even be *correct* for `near`: a
single insert can change the entire top-k ordering, and there is no
incremental HNSW maintenance that is both cheap and correct. The whole
machinery would have to be disabled for fenecdb's headline feature.

Invalidation is at collection granularity: `fenec_changes` says "these
collections changed", the live queries bound to that collection re-run, and
all of it is coalesced into one frame. Anything finer (intersecting id sets)
would cost more than the local query itself.

### `batch()` — one round trip, but not a transaction

```js
await db.batch(async (t) => {
  await t.from('tasks').insert({ title: 'a', status: 'open' });
  await t.from('tasks').where('key', 'x').update({ status: 'closed' });
});
```

There are no transactions in fenecdb (single-writer model) and this batch does
not fake one. The statements run in order **under a single write lock** and
stop at the first error. The gain is not atomicity but two things: one round
trip instead of N, and no other writer slipping in between.

**The local side rolls back fully, the server side cannot.** A batch that
stops halfway says in the response how many were applied:

```json
{"error":"collection `tasks` has no field `nofield`","completed":1}
```

A silent error would permanently separate the client's optimistic local state
from the server.

The body is NDJSON — one `POST /query` body per line. The reason: fenecdb's
JSON parser does not accept nested objects (deliberately), so
`{"statements":[{...}]}` could not be parsed anyway. Instead of writing a
second parser, the boundary is put at the end of a line; by the escaping
rules no JSON encoder can write a bare `\n` into the body.

### Multiple tabs: one stream

Every tab has its own WASM instance. If each opened its own subscription there
would be N copies and N connections. A single **leader** is elected with
`navigator.locks`; the others take the same batches over `BroadcastChannel` and
apply them to their local copies. The apply path is the same, only the transport.

Writes do not go through the leader: every tab sends its own write straight to
the server. Being leader or not only concerns the *read* stream — so the
leader dying does not stop writes; the lock passes to the next tab in line and
it continues from its own cursor.

It is turned off with `leader: false`; without `navigator.locks` (Node) it is
off by itself.

### A dropped connection and the horizon

The client comes back with `?since=<cursor>` and asks for no seed. If the
cursor has fallen behind the ring, the server **reseeds by itself** — rather
than silently sending incomplete events.

```
--changes <n>    entry count of the ring (default 4096, 24 bytes per entry)
```

This number is directly **how far behind a subscriber may fall**. At 100
writes per second, 4096 entries is a ~40 second window. With `persist`, the
image *and the cursor* are written to IndexedDB, so a reopened tab is not
reseeded from scratch — unless the cursor has fallen behind the horizon.

When the server restarts the horizon is set to the counter: a database loaded
from a file has no history, only its present state. A cursor standing exactly
at that point (a quiet restart) gets an empty response; anything behind it is
reseeded from scratch.

### Measures

At the `--changes 4096` default the ring is ~96 KB and its cost on the write
path is one `push` per entry. A subscription does not poll: `fenec-core` knows no
waiting primitive at all (there are no threads in WASM), it only says "the
counter has reached this point"; `fenec-http` ties that to a `Condvar`. A 50 ms
polling loop would take 20 read locks per second for every subscriber and still
add 50 ms of latency.

Every subscription is a connection and a thread, so they are counted
**separately** from ordinary requests (`--http-max-streams`, default 64).
Sharing one ceiling, 100 subscribers would shut the server to plain requests.

### Limits

- **A shape is not a security boundary.** If a changed id does not match the
  shape at all, the subscriber sees it as a deletion; deleting a row that is
  not there locally is harmless, but *which ids changed* leaks. Hiding rows
  needs a separate endpoint (or `--http-read-only` + a token).
- **`bytes` fields cannot be synced.** In JSON they become number arrays with
  no way back; that is the current limit of the HTTP surface, not something
  sync brings with it.
- **In a projected shape the other fields become `null`.** With `select`, the
  local row carries only those fields — the client only knows those anyway.
- **`batch()` does not nest** and the calls inside it return no result (writes
  pile up and go in a single request).
- **Rolling back needs no network, but the server has the last word.** The
  rolled-back local state is overwritten by the server's in the next batch.

---

## Import

Builds a collection from an existing database in a single command: it derives
the schema, recognises vector columns and follows the recommended order —
**bulk write first, index after**.

```bash
fenec import data.sqlite --table docs --into articles \
    --vector embed:384 --index "embed@hnsw(cosine)"

fenec import postgres://user@host:5432/database --table docs \
    --into articles --index "embed@hnsw(cosine)"
```

`--dry-run` prints the derived schema and any warnings, and writes nothing:

```
$ fenec import data.sqlite --table docs --into articles --vector embed:3 --dry-run
source   data.sqlite -> docs
target   articles.fenec -> articles

create collection articles (
  title     text           <- text
  category  text           <- text
  score     int            <- int
  published timestamp      <- datetime
  embed     vector<3>      <- blob
)
  id  <- id (INTEGER PRIMARY KEY)
create index on articles (embed) @hnsw(cosine, m=16, ef_construction=200, ef_search=100)
```

With `--count` it also prints how many rows will be read. It is a separate
flag because counting means a full scan: SQLite keeps the row count nowhere,
and `count(*)` in PostgreSQL reads the table from the start. Silently reading
everything twice would be a bad default.

**SQLite.** The file format is read directly; neither the `sqlite3` library nor
the binary is needed (see *Zero dependencies*). `INTEGER PRIMARY KEY` becomes
the document id, `--vector field:N` turns f32 little-endian BLOBs into
`vector<N>`, and untyped columns are resolved from the first 1000 rows.

**PostgreSQL.** It connects to the live server and streams `COPY ... TO
STDOUT` — no `psql`, no `pg_dump`. Authentication is SCRAM-SHA-256
(`fenec-pg`'s crypto is used in both directions). pgvector's `vector` and
`halfvec` columns are recognised along with their dimensions.

| source | fenecdb |
|---|---|
| `integer` `bigint` `int2/4/8` | `int` |
| `real` `double` `float4/8` | `float` |
| `text` `varchar` `char` `clob` | `text` |
| `blob` `bytea` | `bytes` |
| `boolean` | `bool` |
| `date` `datetime` `timestamp(tz)` | `timestamp` |
| `int[]` `float[]` `text[]` | `[int]` `[float]` `[text]` |
| `vector(N)` `halfvec(N)` | `vector<N>` `vector<N, f16>` |
| `uuid` | `text` |
| **`numeric` `decimal`** | no equivalent — `--cast` required |
| **`json` `jsonb`** | no equivalent — `--cast` required |

### Filtering

`--where` is a **FenecQL expression** and works the same way on both sources: it
is applied after the row is turned into a document and before it is written.
The field names are the target schema's, that is, the same expression you
would later write with `get`:

```bash
fenec import data.sqlite --table docs --into articles \
    --where 'category = "book" and score >= 10 and tags has "rust"'
```

On PostgreSQL there is also `--source-where`: the filter goes into the `SELECT`
inside the `COPY`, so non-matching rows never travel over the wire. This is
**SQL, not FenecQL** — deliberately separate flags, because one is a portable
expression and the other a win pushed to the source. Both can be used together.

```bash
fenec import postgres://... --table docs --into articles \
    --source-where "year >= 2024" --where 'tags has "rust"'
```

With a local `--where` in place, `--limit` is not pushed to the source:
cutting on the server first and filtering locally afterwards would yield fewer
rows than asked for. `--limit` always counts the rows that *pass* the filter.

Types without an equivalent are not silently rounded: you have to say what you
want with `--cast <field>=<type>`. On PostgreSQL, `--cast price=text` preserves
the decimal exactly; in SQLite a `decimal` is already stored as `int` or
`float` at the source, so the exact value is gone from there on.

100 000 rows × 3 dimensions, index build included: ~3.1 s from either source.
All of the options are in `fenec import --help`.

---

## Measurements

Apple M-series, single thread, `--release`. 100 000 documents × 128 dimensions,
clustered embedding distribution (`cargo run --release -p fenec-core --example bench`):

| | |
|---|---|
| write | **9 478 documents/s** (10.6 s) — HNSW indexing included, 8 cores |
| storage | 52.7 MB · 527 bytes/document · 7 segments · no page cache |
| ANN k=10 | p50 **0.139 ms** · p95 0.202 ms · p99 0.226 ms |
| ANN + filter | p50 **0.239 ms** (candidate set n/4) |
| recall@10 | **100%** (against an exact scan) |
| snapshot | 38 ms → 58.4 MB (graph included, +11%) |
| **reopen** | **110 ms** (the graph is validated and restored) |

Without a persisted graph, reopening takes 10.2 seconds (same data, `--example
breakdown`: the index is rebuilt from scratch); that makes a page refresh in
the browser unusable. The graph record speeds it up ~90× — the price is 11%
more space in the image (52.1 → 57.9 MB).

### The ef / recall trade-off

Measured with `cargo run --release -p fenec-core --example bench -- 100000 128
--ef N` — p50 over 100 queries (100 000 × 128, clustered). `--example sweep`
walks the same trade-off on both distributions, but its latency column is the
mean over 50 cold queries, so the two are not directly comparable:

| ef (search) | recall@10 | ANN p50 |
|---|---|---|
| 64 | 99.0% | 0.100 ms |
| **100** (default) | **100%** | 0.131 ms |
| 128 | 100% | 0.169 ms |
| 160 | 100% | 0.172 ms |

The default is 100: it makes recall complete while still being ~7× faster than
the engines compared here. On uniformly random vectors (the pathological case
for ANN — in 128 dimensions all distances converge and no neighbourhood
structure is left) recall at the same ef is around 82%; real embedding models
output clustered data. `--uniform` measures the worst case in the benchmark.

It is tunable per query: `get docs near embed $1 ef 200 limit 10`.
To check correctness, `exact` performs an exact scan.

On the build side `ef_construction` is a lever too (100 000 × 128,
single-threaded measurement):

| ef_construction | build | recall@10 |
|---|---|---|
| 64 | 6 450 rows/s | 94.0% |
| 100 | 5 849 rows/s | 98.5% |
| **200** (default) | 4 929 rows/s | **99.0%+** |

If write speed is critical, `@hnsw(cosine, ef_construction=100)` gains 19%.

## Setup and usage

```bash
make test          # 261 Rust + 61 JS tests
make wasm          # builds WASM for the browser, copies it under web/
make serve         # http://localhost:8787 — the browser console
make bench         # scale measurement
make memory        # memory footprint (--max-memory calibration)
make import-test   # the PostgreSQL arm of import (needs Docker)
make small         # when size comes first: no import, abort-on-panic binary
```

`fenec` includes `fenec import` by default (863 KB). Import is a separate feature
and can be turned off; the `cli` profile also turns panic unwinding into
abort — together they bring the binary down to **636 KB**:

| build | size |
|---|---|
| `cargo build --release -p fenec-cli` (default) | 863 KB |
| `--no-default-features` | 717 KB |
| `--profile cli --no-default-features` (`make small`) | **636 KB** |

`fenec-pg` deliberately stays on the `release` profile: a connection thread that
panics unwinds and takes down only its own session, the server stays up.

### Shell

```bash
./target/release/fenec data.fenec              # interactive
./target/release/fenec data.fenec -c 'get docs limit 5'
echo 'get docs' | ./target/release/fenec data.fenec
```

`.help` `.tables` `.stats` `.functions` `.save <path>` `.checkpoint` `.quit`

```bash
./target/release/fenec types data.fenec > web/fenec-schema.d.ts   # TypeScript from the schema
```

A file with a vector index gets a checkpoint written when it closes, so the
graph is not rebuilt on the next open.

### Browser

```bash
make serve                    # builds + serves -> http://localhost:8787
make serve PORT=3000          # another port
```

By hand:

```bash
cargo build -p fenec-wasm --target wasm32-unknown-unknown --profile wasm
cp target/wasm32-unknown-unknown/wasm/fenec_wasm.wasm web/fenec.wasm
cd web && python3 -m http.server 8787
```

`web/index.html` contains a FenecQL console, a sample data generator, live
statistics and IndexedDB persistence. The **Sync** panel in the sidebar is
optional and does something only when an endpoint is given:

```bash
fenec-pg --file data.fenec --http 127.0.0.1:8080 --http-cors 'http://localhost:8787'
```

The CORS header is required: the page is on `localhost:8787`, the endpoint on
`127.0.0.1:8080` — a different origin. Open the page in two tabs: only one
opens the stream (leader election), the other reads it via `BroadcastChannel`.

Two traps:

- **It does not open over `file://`.** `WebAssembly.instantiateStreaming` and
  module `import`s require HTTP. Any static server will do
  (`python3 -m http.server`, `npx serve`, nginx…).
- **The wasm32 target comes with rustup.** Homebrew's `cargo` has no wasm32
  standard library. `make` picks `~/.cargo/bin/cargo` by itself; if you build
  by hand you need `rustup target add wasm32-unknown-unknown`.

To use it on your own page all you need is `web/fenec.js` and `web/fenec.wasm`:

```html
<script type="module">
  import { Fenec, persist, restore } from './fenec.js';

  const db = await Fenec.open('./fenec.wasm');
  db.run('create collection docs (title text, embed vector<384> @hnsw(cosine))');
  db.run('put docs {title: $1, embed: $2}', ['hello', embedding]);

  const { rows } = db.run('get docs near embed $1 limit 5', [queryVector]);

  // or with the builder -- for dynamic filters and parameter binding
  const near = await db.from('docs').near('embed', queryVector).limit(5).rows();

  await persist(db);            // write to IndexedDB
  await restore(db);            // restore in the next session
</script>
```

If a replica that syncs with the server is wanted, `sync` instead of `Fenec.open`:

```html
<script type="module">
  import { sync } from './fenec.js';

  const db = await sync({
    url: 'https://api.example.com',
    shapes: [{ collection: 'tasks', where: { status: 'open' }, key: 'key' }],
    persist: 'tasks',           // the image *and* the cursor are stored
  });
  await db.ready();

  db.live(db.from('tasks').order('priority', 'desc'), draw);
  await db.from('tasks').insert({ title: 'new', status: 'open' });
</script>
```

For the full builder and type generation: [Query builder](#query-builder); for
the sync layer: [Sync](#sync-local-replica-and-server).

The same file works in Node; there `fetch` cannot resolve a relative path, so
the module bytes are handed over directly:

```js
import { readFile } from 'node:fs/promises';
const db = await Fenec.open(await readFile('./fenec.wasm'));
```

### PostgreSQL server

```bash
./target/release/fenec-pg --listen 127.0.0.1:5433 --file data.fenec
./target/release/fenec-pg --ping            # 0 = up (health check)

# also open the HTTP/JSON endpoint: same process, same file
./target/release/fenec-pg --file data.fenec --http 127.0.0.1:8080
```

Resource ceilings: `--max-connections` (100), `--max-message` (64 MiB),
`--idle-timeout` and `--max-memory` (off). On the subscription side
`--http-max-streams` (64), `--http-keepalive` (20 s) and `--changes` (4096
entries). For all of them, `fenec-pg --help`.

Watching the stream by hand needs no driver:

```bash
curl -N 'http://127.0.0.1:8080/articles/changes?year=gte.2024'
# event: seed
# data: {"seq":12,"rows":[...]}
```

### Container

The `Dockerfile` is two-stage: it builds statically with musl and puts the
result into `scratch`. There are no dependencies and the binary does the
health check itself (`--ping`), so the runtime image holds nothing but the
binary — no shell, no package manager, no libc. The image is **1.55 MB**, and
since no target is written in, arm64/amd64 come out of the same file.

```bash
make docker                     # the image
make docker-run PGPASS=secret   # 127.0.0.1:5433, named volume
psql -h 127.0.0.1 -p 5433 -U fenec
```

or `docker compose`:

```bash
FENECPG_PASSWORD=secret docker compose up -d
```

Three behaviours feel different in a container:

- **A password is mandatory.** The default `CMD` listens on `0.0.0.0`; with
  authentication off, `bind()` on a non-loopback address errors out and exits.
  Either `FENECPG_PASSWORD` is given (a Docker secret works too) or `--insecure`
  is added deliberately. There is still no TLS: the port is published to
  `127.0.0.1:5433`, and an open network needs a TLS terminator in front.
- **The memory limit is chosen from the data.** With no page cache there is no
  knob to tune either: the open peak is ≈ 2× the file, `compact`/`checkpoint`
  ≈ 3× the file. If the limit goes below that, the cgroup sends `SIGKILL` — the
  clean shutdown does not run and up to one `--sync` interval of writes is
  lost. To bring that forward, set `--max-memory` to a third of `mem_limit`:
  writes stop with `53200`, the process stays up and the `del` + `compact`
  path stays open.
- **The restart cost depends on how it shut down.** A container stopped with
  `docker stop` (`SIGTERM`) writes a checkpoint; the graph stays in the file
  and the next open takes 110 ms. With `kill -9` or a cgroup OOM that does not
  happen and the graph is rebuilt in 10.2 s (100 000 × 128). To collect dead
  bytes as well, `make docker-compact PGPASS=...` is called separately.

The health check is `fenec-pg --ping`: it connects, authenticates and exits. It
reads the password from `FENECPG_PASSWORD`; measured at ~100 ms. Because the
listener binds *after* the file is opened, "healthy" also means "the index is
ready" — `--start-period` has to be long enough to wait for the graph build.
If you change the port in `CMD`, `--listen` has to be added to `HEALTHCHECK`.

No shell: when debugging, `docker exec ... sh` does not work; what is left is
`docker logs` and `docker run --rm fenecdb --help`.

`SIGTERM` is caught and a final `sync` runs before exit; the handler is
installed explicitly, so it works as PID 1 as well and needs no `init`/`tini`
(measured `docker stop`: 0.30 s).

**A single instance.** If two containers load the same file into memory and
write it, the file is corrupted. On Kubernetes you need a `StatefulSet` not a
`Deployment`, `replicas: 1`, `strategy: Recreate` and a `ReadWriteOnce` volume
— `RollingUpdate` keeps two pods up together for a moment. There is a
connection ceiling (`--max-connections`, default 100) but no pool: an excess
connection does not wait, it returns `53300`. Put pgbouncer in front to queue.

### Embedded (Rust)

```rust
use fenec_core::prelude::*;
use fenec_ql::parse;

let mut db = fenec_core::fs::open("data.fenec")?;
for stmt in parse("get docs near embed $1 limit 5")? {
    let r = db.execute_with(&stmt, &[Value::Vector(embedding)])?;
}
```

---

## Layout

```
crates/
  fenec-core/   storage, HNSW, plan executor, plugin registry, JSON,
                calendar arithmetic, change stream                 (no dependencies)
  fenec-ql/     FenecQL lexer + parser                             (core only)
  fenec-wasm/   the browser ABI                                    (no wasm-bindgen)
  fenec-pg/     PostgreSQL v3 wire protocol + plugin (server and client)
  fenec-http/   HTTP/JSON endpoint: REST surface, raw FenecQL, subscription (SSE)
                                                                   (core+ql only)
  fenec-import/ SQLite file format reader + PostgreSQL COPY source
  fenec-cli/    the `fenec` shell, `fenec import`, `fenec types`
web/            the fenec.js client (query builder + sync layer),
                fenec.d.ts types, fenec.test.js + fenec.sync.test.js
                (node --test) + the browser console
```

## File format

A single file, replayed in a single pass:

```
"FENECDB\x01"
  [6][u64 counter][u64 body length]        change counter (optional)
  [1][collection-id][length][schema]       create collection
  [7][collection-id][length][next-id]      id counter (optional)
  [5][collection-id][length][schema]       schema change (index)
  [2][collection-id][length]               drop collection (empty body)
  [3][collection-id][length][records]      data
  [4][collection-id][length][field][graph] HNSW graph (optional)
```

Records: `[op][doc-id][length][fields]`. A half-written final record is
truncated on open — no separate recovery step is needed after a crash.

Apart from the counter record they all share one layout:
`[kind][collection-id][length][body]`. The length is written even in a `drop`
record with an empty body, and the read side **has to consume it**; otherwise
the leftover byte is read as the next record kind and the file stops opening.

The graph record is **not a cache but derived data**: the version, the
dimension, the node count and the link bounds are validated; if they do not
hold it is ignored and the index is rebuilt from scratch. So a corrupt or
stale graph cannot cause data loss. The graph is written only during
`snapshot`, `compact` and `checkpoint` — not on the normal write path.

The counter record is **at the front and fixed width**, both deliberate. At the
end, a corrupt or half-written tail would break opening: the end of the file is
exactly where the graph stops, and the graph is derived data — a record that
errors out in that region would take that tolerance back. Fixed width because
the body length is known only after the body is written; the placeholder is
filled in place, whereas a uvarint would mean shifting the whole image. The
body length means "this is where this image's own records end" — every record
after it was written *after* the checkpoint and carries the counter forward.
Old files without the record load fine, the counter counted from the records;
the reverse does not, an old binary cannot open it (`unknown record kind 6`).

**The id counter record exists because of `compact`.** A document id is
normally derived from the records: a replay takes one more than the largest id
it saw. Compaction throws tombstones away, so that derivation falls short
there — the highest deleted id disappears from the image entirely and would be
**handed out again** on the next open. An id coming back silently binds
everything holding on to it (links handed out, the row a subscriber holds) to
the wrong document. The record is per collection and comes right after the
schema; only `snapshot` writes it, the WAL path does not need it because the
tombstones are still there. Old files without the counter still load correctly.

## Limits

### Out of scope

Deliberately not done: transactions (single-writer model), JOIN, subqueries,
schema migration, multi-writer replication, SQL.

Replication exists in **one direction only**: the server is authoritative,
clients read it (see [Sync](#sync-local-replica-and-server)). Multi-writer
merging, CRDTs and a long-lived offline write queue are out of scope — that is
what fits the single-writer model; the other would be a different database.

There are deliberate gaps on the type side too: **there is no decimal** —
`float` is binary floating point, unfit for money; the answer is an `int` in
cents. **There is no separate UUID type**, `text @hash` is functionally
enough. **There are no nested objects (json)**: a list is homogeneous and an
object value is refused on purpose; a field to be filtered should be a field.

### Fixed limits

The following are ceilings baked into the code. Exceeding the first two makes
the query **error**: a silently cut result is a wrong answer believed right.

| limit | value | note |
|---|---|---|
| `near` result (`limit + offset`) | **10 000 rows** | a query error when exceeded; without `limit` the default top-k applies |
| expression depth | **512 levels** | parentheses *and* `and`/`or` chains count; a query error when exceeded |
| session stack (`fenec-pg`) | 8 MiB | virtual; the deepest expression wants ~750 KiB in release, ~5 MiB in debug |
| segment size | 8 MiB | sealed when full, a new one is opened |
| single document payload, segment offset | 4 GiB | the location record is `u32` |
| vector index node count | 2³² | the node id is `u32`; in practice memory runs out first |
| level-0 degree (`m0 = 2m`) | 65 535 | `u16` counter → `m` ≤ 32 767 |
| bulk insert batch | 64–512 vectors | `graph/16`, clamped to this range |
| parallel build threshold | 1024 vectors | below it, the serial path |
| gap in an externally given `id` | 4096 | a larger jump falls into a sparse map, not a dense array |
| `timestamp` | 1 ms, i64 (±2.9×10⁸ years) | microseconds are truncated, the time zone is UTC |
| `vector<N, f16>` | ±65 504, ~3 decimal digits | outside it silently becomes `inf`/`0` |
| `fenec-pg` message size | 64 MiB | changed with `--max-message`; the protocol ceiling is 1 GiB |
| `fenec-pg` startup packet | 10 000 bytes | the same as PostgreSQL; the only allocation before authentication |
| `fenec-pg` column / parameter count | 32 767 | the protocol's `i16` counters |
| change ring | **4096 entries** | changed with `--changes`; how far behind a subscriber may fall. 24 bytes per entry |
| concurrent subscriptions | **64** | `--http-max-streams`; each is a connection and a thread |

### Memory and scale

**The whole database is in memory**, not just the vector arena: the segment
bytes sit in RAM in their on-file form (this is the other side of saying there
is no page cache). That has three consequences:

- **The open peak is ≈ 2× the file.** The file is read in one go, then copied
  into segments. Measured: a 30.5 MB vector-free file → 65.5 MB peak RSS.
- **The `checkpoint`/`compact` peak is ≈ 3× the file.** The whole image is
  produced in memory and written to a side file. On the same file: 95.8 MB.
- **The open cost is not constant, it is proportional to the data size**: for
  100 000 × 128 with the graph in the file, ~110 ms. In a process that runs
  fewer than five vector queries, SQLite gives the faster total.

The vector arena is the largest item in that total: 1 M × 768 dims ≈ 3 GB,
1.5 GB with `f16`. In the browser the WASM32 address space (4 GB) is the upper
bound — the 2–3× factors above come out of it too, so the real ceiling is lower.

**What can be measured.** `Database::memory_bytes()` sums the segment bytes,
the offset indexes, the vector arenas and the graph links from counters;
`fenec-pg`'s `--max-memory` ceiling uses it. It is not RSS: allocator slack,
HNSW build buffers, upper-level neighbour allocations (~3% of l0) and the
2–3× transient peaks are outside it. Measured with `make memory`:

| | measured footprint | peak RSS | ratio |
|---|---|---|---|
| 100 000 × 128 | 128.2 MB | 172.9 MB | 74% |
| 200 000 × 4 | 57.6 MB | 97.2 MB | 59% |

So the ceiling is not a guarantee but an early warning. **A third** of the
container memory is a sensible start: it covers both this 60–75% ratio and
`compact`'s 3× peak.

### Index build, open and maintenance

- **The graph is derived data and is written only during `snapshot`,
  `compact` and `checkpoint`** — not on the normal write path. Without a graph
  in the file it is rebuilt on open: 0.90 s on every open at 20 000 × 32
  (0.01 s with the graph). At 100 000 × 128 that means the entire graph build:
  10.2 s to load without it, 110 ms with it (see [Measurements](#measurements)).
- Who writes the checkpoint: the `fenec` shell when leaving an interactive/stdin
  session, and `fenec-pg` when it gets the shutdown signal — both only if a
  vector index exists (`--no-checkpoint` turns it off in `fenec-pg`). **The
  one-shot `fenec file.fenec -c '...'` call does not write one.** With `kill -9` or
  a cgroup OOM there is none either: the graph is rebuilt on the next open, so
  what is lost is derived data. Collecting dead bytes is `compact`'s job.
- **`compact` is not a garbage collection but a full rebuild**: even with zero
  dead bytes, every index is built from scratch (20 000 × 32 → 0.94 s; 0.05 s
  on a vector-free text collection of the same size). Writes block throughout.
- A delete is a tombstone; the graph node stays in memory until `compact`.
  Updating a document also marks the old node deleted and adds a new one, so in
  collections whose vectors are updated the graph grows until `compact`.
- Documents inside one batch cannot see each other as neighbours. The batch
  size is 1/16 of the graph size (64–512), so there is no measured recall
  loss, but very small collections fall to the serial path (<1024 vectors).
- The HNSW build is parallel within a batch; because writing the links is
  serial the speed-up is not linear in core count (~1.9× on 8 cores).
- File writes are buffered; if the process crashes before `sync`/`checkpoint`
  is called the last writes can be lost. In embedded use calling `sync` is the
  caller's job; `fenec-pg` does it via the `--sync` policy and a shutdown hook.

### Query behaviour

- **Only `@hash` equalities in an `and` chain reach the index.** `>=`, `~`,
  `has`, `in` and anything under an `or` is a full scan: every matching row is
  decoded and evaluated. There is no separate index for text.
- The cost of a filtered `near` is dominated by extracting the filter set, not
  by the vector search: of the ~6.5 ms measured on 100k documents, nearly all
  is the filter scan (an unfiltered ANN is 0.2 ms). With a selective filter the
  exact-scan path is taken, so recall is 10/10.
- **There is no top-k for `order`**: the key of every matching row is
  extracted and sorted, and `limit` is applied afterwards. The cost depends on
  the number of matches, not on `limit`.
- `near` and `order` cannot be used together; `near` determines the ordering
  by similarity.
- The query vector of `near` can be given in three forms: a vector, a list
  and pgvector's text representation (`near embed '[1,0,0]'`) — the last is the
  natural form when typing by hand from psql. None of the three breaks
  silently: unparsable text and a non-numeric component both error.
- **`put` does not check that a vector is finite.** A NaN or infinite
  component enters the index and makes the distances meaningless; the check is
  only on the `fenec import` path. In a `vector<N, f16>` field an out-of-range
  value also silently becomes `inf` (±65 504) or drops to zero (below ~6×10⁻⁸).
- `now()` does not work in the browser: the `wasm32-unknown-unknown` target has
  no clock, the call errors. Time is passed as a parameter via `Date.now()`.

### `fenec-pg`

- It does not speak TLS. The password is protected by SCRAM but the data
  flows in the clear; on an open network it has to go behind a TLS terminator.
  Non-loopback + unauthenticated listening is refused without `--insecure`.
- It does not support transactions: `BEGIN`/`COMMIT` are accepted and have no
  effect. The single-writer model is per-statement atomicity.
- It sees a cancellation while waiting on the lock and between batched
  statements; a running statement (a long `compact`) is not cut in the middle.
- **A ceiling, not a pool**: every connection is an operating system thread.
  `--max-connections` (default 100) rejects what goes over it with `53300` but
  does not make it wait — if you want a pool that queues, put pgbouncer in
  front. When a thread cannot be spawned (RLIMIT_NPROC, pids cgroup) only that
  one connection drops; an accept error is not fatal either, though the server
  stops after 64 errors in a row.
- An idle session lives **forever** by default; `--idle-timeout <s>` turns it
  on. The timeout applies while waiting for the next message and equally in the
  middle of a half-received message: both are signs of a dropped connection.
- Session threads get an **8 MiB** stack. `thread::spawn`'s 2 MiB default
  leaves 2.7× headroom for a 512-level expression in release but is not enough
  in a debug build — and a stack overflow is not a catchable panic, it is the
  process calling `abort`. It is virtual space: with 100 idle connections the
  measured RSS is 5.2 MB, ~36 KiB per connection, independent of the stack.
- **The row count of `Execute` is ignored**: the portal always runs to the
  end and no `PortalSuspended` is sent. Clients using `fetch_size` or a named
  cursor get the whole result in one go.

### `fenec-http`

- **It is not a separate process.** It opens as a second listener in the same
  process via `fenec-pg --http`; there is no standalone binary. The reason is the
  single-writer model: if two processes open the same file it is corrupted.
- No TLS, the same rule as `fenec-pg`. Without a token it will not bind to a
  non-loopback address (bypassed with `--insecure`). The token is a single
  shared secret: no user separation, no scopes, no expiry.
- `Transfer-Encoding: chunked` is not supported (411). The body is limited to
  64 MiB and the header block to 64 KiB; a request above that gets 413/431.
- Pagination is `limit`/`offset`; there is no cursor and no `Range` header.
  The response is built in one piece: a very large `select` holds that much
  memory on the server.
- A field whose name matches a clause name (`select` `order` `limit` `offset`
  `count` `where`) cannot be filtered from the query string; the `POST /query`
  and `fenec-pg` paths are unaffected by this limit.
- There is no `ETag`, no `Last-Modified` and no conditional request: responses
  are not marked cacheable.
- **A subscription is one-way** (SSE): the server sends changes and the client
  does its writes with ordinary `POST`/`PATCH`/`DELETE`. There is no WebSocket;
  for the same job it would demand framing, masking and ping/pong.
- A subscription does not accept `order`/`limit`/`offset`/`count` (400): a
  shape is a set, not a window; an incremental diff cannot express a moving one.
- Every subscription holds a thread and is bounded by `--http-max-streams`;
  they are counted separately from `--max-connections`.
- A write to **any** collection wakes a subscriber. That is not noise but a
  necessity: the counter and the ring are shared across all collections, so if
  the subscriber of a quiet collection did not wake, its cursor would stay put
  and it would be reseeded once other people's writes overflowed the ring.
  Waking moves the cursor forward; on a change that does not match its shape no
  empty batch is sent. The cost is one read lock and an empty scan.
- `POST /batch` is not atomic (no transactions in fenecdb): it stops at the first
  error and `completed` in the response says how many were applied. No rollback.

### Import

- One table → one collection works; JOINs and multi-table migration are out of
  scope (there is no JOIN in fenecdb anyway).
- `fenec import` is in the default build but is a separate feature: a binary
  built with `--no-default-features` has no such subcommand and says so and
  exits when called (see [Setup and usage](#setup-and-usage)).
- `--where` sees only the built-in functions (`lower`, `len`, `coalesce`, …);
  plugin functions cannot be used. The filter is evaluated without borrowing
  the target database, so that writing can happen in the same loop.
- A SQLite file with an unprocessed WAL is not read — the main file shows
  stale data, and stopping beats silently returning it. Run
  `sqlite3 <db> "PRAGMA wal_checkpoint(TRUNCATE)"` first. `WITHOUT ROWID`
  tables are not read either.
- `fenec import` does not speak TLS (like `fenec-pg`) and does not support md5
  authentication; scram-sha-256 or cleartext is required.
- PostgreSQL's `timestamp(tz)` values are truncated to milliseconds: fenecdb's
  `timestamp` resolution is 1 ms and microseconds are silently dropped.
- If a column taken as a vector has a non-finite component (NaN, infinity)
  the import stops. A NaN makes a distance incomparable; even if the index
  accepted such a vector the result would be meaningless, so it stops and says
  which row it was in.
- An import that stops halfway cannot be rolled back (no transactions). If
  `fenec import` created the file itself it deletes it; when writing into an
  existing file the collection can be left half done.

## Contributing

`CONTRIBUTING.md` has the setup, the narrower test invocations and the
invariants a change must not break.

## License

Apache-2.0. See `LICENSE`.
