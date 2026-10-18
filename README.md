# fenecdb

[![ci](https://github.com/fenecdb/fenec/actions/workflows/ci.yml/badge.svg)](https://github.com/fenecdb/fenec/actions/workflows/ci.yml)

An embedded document database for an app's data, from the page to the
server: state its screens follow live, a replica on the device that syncs
with the server, many clients writing into one file through one writer
and sharing their disk syncs, a server with no cache in front of it, a file
per tenant in every region, and search built in --
BM25 over text with typos, highlights and facets, and vectors for meaning.
Written in Rust with no dependencies, it runs inside a web page as 301 KB of
gzipped WebAssembly, in an iOS, Android or Flutter app as a file on the
device, in a Rust process, or as a server any language reaches over HTTP,
and it has its own query language (**FenecQL**).

**[fenecdb.com](https://fenecdb.com)** — the website and documentation. The
home page shows each part at work, and its playground runs the real
WebAssembly module in your browser with fenec studio over it.
Source in [`site/`](site/): `make site-serve` runs it locally, `make site-deploy`
publishes it to Cloudflare Workers.

```
create collection orders (
  customer  text      @hash,
  status    text      @hash,
  total     int,
  placed    timestamp @sorted
)

get orders select id, total, placed
  where customer = $1 and status = "open"
  order placed desc
  limit 20
```

The same query from JS, and kept live: a write that changes what it reads
draws the screen again -- no ORM, no npm, no build step:

```js
const open = db.from('orders')
  .select('id', 'total', 'placed')
  .where('customer', customer)
  .where('status', 'open')
  .order('placed', 'desc')
  .limit(20);

const rows = await open.rows();      // once
const stop = db.live(open, draw);    // and again after every write it reads
```

A write reads the row it writes, and can be made to find it. Sent as one
`/batch`, a debit that finds too little money is refused (412, `unmet`,
`"at": 0`) before the credit runs, and a credit to an account that is not
there (`"at": 1`) puts the debit back:

```
set accounts {balance: balance - $3} where name = $1 and balance >= $3 require 1
set accounts {balance: balance + $3} where name = $2 require 1
```

Search is in the same query: the words ranked by BM25, a typo and a word
half typed found, what matched marked, and how many results each brand
has -- for a results page and its filter sidebar:

```
get products select name, highlight(name, "<mark>", "</mark>")
  match name $1 fuzzy prefix
  where price < 500
  facet brand top 10, color
  limit 20
```

And by meaning, where a field holds an embedding: `near` finds the nearest
rows, with a filter planned beside it:

```
create collection articles (
  title     text,
  body      text  @text,
  tags      [text],
  year      int   @hash,
  embed     vector<768> @hnsw(cosine, m=16, ef_construction=200)
)

get articles select title
  where year >= 2024 and tags has "rust"
  near embed $1
  limit 10
```

Or both rankings at once: BM25 and the vectors each find their own
candidates, and reciprocal rank fuses the two lists, so a document either one
missed still counts:

```
get articles select title
  match body $1
  near embed $2
  fuse
  limit 10
```

Lexical recall and an exact vector reordering on top of it — the second stage
reads the stored vectors, so this path needs no HNSW graph at all:

```
get articles select title
  match body $1
  rerank embed $2 candidates 500
  limit 10
```

---

## Which one, when

**fenecdb** makes sense when the database should run beside the code that uses
it — in the browser, in the process, or as one small server — and search
belongs in it rather than in a second system: an app whose screens follow
its data and keep it offline, a server many clients read and write with no
cache in front, a service with a file per tenant in one region or several,
and search over its own rows, a store's, a docs site's or an agent's
memory. The scale limit is **the indexes fitting in memory**: the documents
stay in the mapped file, so a server's memory follows its indexes, not its
data ([Limits](https://fenecdb.com/docs/limits#memory-and-scale)).

**SQLite** when you need relational data, SQL with JOINs and a mature
toolchain. Also in very short-lived processes: for a tool that runs
fewer than five queries and exits, fenecdb's open cost never amortises.

**PostgreSQL** (with pgvector for vectors) for multi-writer systems shared
over a network that need ACID. On maturity, concurrency and ecosystem the comparison is not even
worth making — fenecdb is not aiming at that job. A table can move the
other way, though: `fenec import` copies one from PostgreSQL, and `--follow`
keeps it following the table's commits.

The numbers behind the comparison, and the method that produced them, are in
[Benchmarks](https://fenecdb.com/docs/benchmarks). On YCSB's
workloads in process, fenecdb does B with 16 threads at 545 k operations a
second against SQLite's 47.9 k, and C on one thread at 438 k against
279 k, its file compacted on its own as updates grow it ([YCSB](https://fenecdb.com/docs/benchmarks#ycsb));
over HTTP, `fenec-server` does B at 16 clients, durable, at 18.2 k against
PostgreSQL's 13.0 k, both in Docker. At a million vectors it answers the
nearest ten at 99.1% recall in 0.141 ms, against pgvector's 98.6% in 2.076
([At scale](https://fenecdb.com/docs/benchmarks#scale)).

---

## Install

```bash
git clone https://github.com/fenecdb/fenec && cd fenec
make test     # the Rust suite, then the JS suite
make serve    # builds the wasm and serves the browser console on :8787
```

Five minutes from clone to your first query:
**[Quickstart](https://fenecdb.com/docs/quickstart)**.

**[Examples](examples/)**: one small app, Notes, in every language -- the
browser, React, Node, Python, Go, .NET, Rust, SwiftUI, Jetpack Compose and
Flutter -- each a folder to open and run, with a live list, filters, and
full-text and vector search.

**Shell.** `cargo build --release -p fenec-cli`, then:

```bash
./target/release/fenec data.fenec              # interactive
./target/release/fenec data.fenec -c 'get docs limit 5'
```

**Browser.** Dependency-free ES modules and the wasm file beside them:

```js
import { Fenec } from './fenec.js';
const db = await Fenec.open('./fenec.wasm');
```

301 KB of gzipped WebAssembly and a 59 KB gzipped client — no wasm-bindgen, no
build step. An app whose queries run on a server can import
`@fenecdb/web/client`, 9 KB brotli in its bundle, which cannot pull in the
engine, sync or storage ([which package](https://fenecdb.com/docs/javascript#packages)). With live
queries (`db.live`, React's `useLiveQuery`) it can be an app's whole state,
no server: [state in the page](https://fenecdb.com/docs/javascript#state). [JavaScript client](https://fenecdb.com/docs/javascript).

**Server.** `fenec-server` serves a file, or a directory of tenants, over
HTTP and JSON: REST routes, FenecQL through `POST /query`, all-or-nothing
`/batch`es, subscriptions over SSE and change data capture. Every language
reaches it with its own HTTP client
([languages](https://fenecdb.com/docs/languages)), and these have a client
of their own, each its standard library alone:

| Language | Package | From |
| --- | --- | --- |
| Python | `pip install fenecdb` (with the LangChain and LlamaIndex stores) | [`integrations/python`](integrations/python) |
| JavaScript | `npm install @fenecdb/web` (`connect`) | [`web/`](web) |
| Go | `go get github.com/fenecdb/fenec/integrations/go` | [`integrations/go`](integrations/go) |
| .NET | `dotnet add package FenecDb` | [`integrations/dotnet`](integrations/dotnet) |

```bash
make server TOKEN=secret
curl -H 'Authorization: Bearer secret' -d '{"query": "collections"}' \
  http://127.0.0.1:8080/query
```

One process writes a file -- two opening one would corrupt it -- so
replication, the graph keeper and the mirror are threads of it. A second
`fenec-server` follows it as a read replica with `--replica-of`: it is sent
the writes on the primary's disk, serves reads, refuses writes with 403, and
is promoted by hand; `fenec backup`, `fenec archive` and `fenec restore --to
<time>` take a running database whole, keep its writes, and rebuild it as it
stood at a moment -- with `--tenants`, every tenant of a `--dir` node over
one connection. With `--follow postgres://...` it mirrors a PostgreSQL
table into its file as the table commits, and serves the mirror while it
follows. [Server](https://fenecdb.com/docs/server) ·
[HTTP endpoint](https://fenecdb.com/docs/http) ·
[Replication](https://fenecdb.com/docs/replication).

Past one machine, a `--dir` node holds a file per tenant and `fenec-shard`
routes each request to its tenant's node, gives each tenant a replica on
another node, and fails a node over on a lease. Nodes have a region and a
zone: a tenant lives in one region, its replica in another zone of it, and
a region may keep its own directory, so it leases and fails over its own
nodes while cut off from the rest. One Cloudflare Worker in front gives
every tenant one URL, and a tenant's read replica in another region answers
the reads a client there marks, which would each pay the trip home
otherwise (Frankfurt to Singapore is 158 ms), never from before that
client's own write. `deploy/` is the kit to run it on machines of your own
-- systemd units, alert rules and the operator's runbook -- and `make drill`
kills a node and a router of a two-region deployment in Docker and checks
that no acknowledged write was lost.
[Tenants and sharding](https://fenecdb.com/docs/sharding) ·
[Regions](https://fenecdb.com/docs/sharding#regions) ·
[One URL for every region](https://fenecdb.com/docs/sharding#global-entry) ·
[Read replicas](https://fenecdb.com/docs/sharding#read-replicas) ·
[Deploying it](deploy/RUNBOOK.md).

**Container.** 2.99 MB, and the `Dockerfile` is two-stage: static musl build
into `scratch`, so the runtime image holds the binary and nothing else — no
shell, no package manager, no libc.

```bash
docker pull ghcr.io/fenecdb/fenec-server:0.1.10     # published, multi-arch
make docker && make docker-run TOKEN=secret   # or build it yourself
```

**Mobile and native apps.** The engine as a native library
(`crates/fenec-ffi`, a C ABI) keeps the database in a file on the device:
Swift for iOS and macOS, Kotlin for Android and the JVM, Dart and Flutter,
each with the query builder and live queries driving the UI. The file can be
a replica a server keeps in step -- reads local, writes optimistic and
queued while offline, over the platform's own HTTP client and its TLS -- and
`Fenec.connect` asks a server with no file at all.

| Language | Package | From |
| --- | --- | --- |
| Swift | SwiftPM: `github.com/fenecdb/fenec`, product `FenecDB` | [`integrations/swift`](integrations/swift) |
| Kotlin | `com.fenecdb:fenecdb-android` (the AAR), `com.fenecdb:fenecdb` (the JVM) | [`integrations/kotlin`](integrations/kotlin) |
| Dart | `flutter pub add fenecdb_flutter`, or `fenecdb` in Dart alone | [`integrations/dart`](integrations/dart) |

```swift
let db = try await Fenec.open(path: path)
let open = try await db.from("todos").where("done", false).rows(as: Todo.self)
let live = LiveQuery(db, try db.from("todos").where("done", false), as: Todo.self)   // for SwiftUI

// The same, a replica the server keeps in step: the one line that changes.
let db = try await Fenec.sync(url: "https://api.example.com", token: jwt,
                              shapes: [Shape("todos", key: "key")], path: path)
```

[Mobile and native apps](https://fenecdb.com/docs/mobile).

**Embedded in Rust.** `fenec-core` is the engine as a library:
[Embedded Rust](https://fenecdb.com/docs/embedding).

---

## What it does

| | |
|---|---|
| **Documents** | `insert` (refuses a taken id), `put` (upsert), `set` and `del` by filter, a batch landing whole |
| **Atomic batches** | `POST /batch` and the browser's `run` of several statements land whole or not at all; an `Idempotency-Key` makes a write once |
| **Live state and sync** | a query a screen follows: a write runs again every live query reading what it wrote (`db.live`, `useLiveQuery`, SwiftUI observables, Flows, Streams) · a replica in the page or the app that reads with no network, queues its writes in its file and sends each once under an `Idempotency-Key`, the server owning the data ([Sync](https://fenecdb.com/docs/sync)) |
| **Schema** | `alter collection` adds, drops and renames a field without rewriting a document |
| **Schema in code** | tables declared in TypeScript Drizzle's way, or a `schema.fenecql` in any SDK, checked at every open: what only adds is applied, a drop, a rename or a type change refused until a migration says, run once and recorded; `fenec types` writes TypeScript, Python, Go or C# from a database or a schema file |
| **Types** | `bool` `int` `float` `text` `bytes` `timestamp` `[type]` `json` (objects, lists and scalars; a path such as `meta.source.rank` reads into it in `where`, `select`, `order` and `set`; imported from PostgreSQL's `jsonb`) `geo` (a point, `[lon, lat]`; imported from PostGIS) `vector<N[, f16]>` `sparse<N>` |
| **Indexes** | `@hash`, `@unique` (a second document holding a value refused, `null` aside), `@sorted`, `@sorted(a, b desc)` and `@unique(a, b)` over several fields, `@ttl(30m)`, `@text(k1=.., b=.., prefix=..)`, `@geo`, `@hnsw(metric, m=.., ef_construction=.., ef_search=.., quant=int8\|bit)`, `@inverted`; `@hash`, `@unique` and `@sorted` on a path into a `json` field too |
| **Operators** | `= != < <= > >=`, `~` (text contains, case-insensitive), `has` (list contains), `in [..]`, `in (get ...)` (another collection's rows, run once, up to 100 000 values), `is null`, `between`, `like` and `ilike` |
| **SQL's spellings** | `INSERT INTO … VALUES`, `UPDATE … SET a = …`, `IN (…)`, `BETWEEN`, `LIKE` and `ILIKE`, `ON CONFLICT DO NOTHING` and `DO UPDATE SET`, `IS DISTINCT FROM`, single quotes and `\|\|` read as the FenecQL they mean, through every client; `CREATE TABLE`, `JOIN` and the rest refused with the FenecQL to write ([From SQL](https://fenecdb.com/docs/sql)) |
| **Aggregates** | `count(*)` `sum` `avg` `min` `max`, whole or per `group`, ordered and paged by any of them |
| **Relations** | `lookup` — a collection's matching documents attached per row, `limit` counted per parent, keyed by one field or several (`on (org, order) = (org, id)`), chainable to 8 levels |
| **Expiry** | `seen timestamp @ttl(30m)` — a row gone that long after its time: out of every read at once, deleted by the server's sweeper as ordinary deletes |
| **Collation** | `order name collate und` — Unicode's order for every script, as ICU's root orders it (PostgreSQL's `und-x-icu`); `collate tr` Turkish (`ç` after `c`, `ı` before `i`, `tr-x-icu`); a field declared in one pages by its last row; bytes otherwise |
| **Case-insensitive text** | `email text nocase @unique` — compared by its Unicode case fold wherever it is compared (`=`, its indexes, `order`, `group`, `facet`), kept as written, PostgreSQL's `citext`; Turkish's `I`/`ı` and `İ`/`i` under `collate tr` |
| **Text search** | `match` ranked by BM25 over `@text` · `fuzzy` for a word typed wrong and `prefix` for one half typed · `highlight()` and `snippet()` marking what matched · `facet` counts beside the page, disjunctive and by ranges · Chinese, Japanese, Korean and Thai split into runs of characters |
| **Places** | `distance(loc, p) <= 500` and `within(loc, [w, s, e, n])` over a `geo` field, through `@geo`, nearest first with `near` (in metres) — Redis's `GEOSEARCH`, the metres its `GEODIST`'s ([Points and distances](https://fenecdb.com/docs/geo)) |
| **Instead of Redis** | counters (`set c {n: n + 1}`), rate limits (fixed and sliding windows, token buckets), locks with fencing tokens, sessions and caches under `@ttl`, leaderboards, sets, HyperLogLog, job queues with leases and a claim held until a job comes, delayed jobs, streams, pub/sub, idempotency keys — each a statement or two a test runs; no LRU eviction, no Lua ([Instead of Redis](https://fenecdb.com/docs/redis)) |
| **Functions** | `lower upper len coalesce now timestamp cosine l2 dot norm normalize distance within` + plugins |
| **Vector search** | `near` over `vector<N[, f16]>` (HNSW, with `int8` and `bit` codes; exact by dot product over a `sparse<N>` such as SPLADE's), by `cosine`, `l2` or `dot` · `rerank` (exact vector reordering of `match` candidates, no graph needed) · `fuse` (`match` and `near` ranking together, by reciprocal rank) |
| **Interfaces** | FenecQL · a query builder in seven languages · REST/JSON + SSE · WASM C ABI · a native C ABI for apps · change data capture (`/_changes`, every write on disk as a JSON line, resumable) · import from SQLite and PostgreSQL |
| **Agents** | an MCP server (`fenec-server --mcp`, `fenec mcp` over stdio): `schema`, `search` (words, an embedding, or both fused), `query`, `get`, and `write` and a memory (`remember`, `recall`) where asked for, held to the token's rules |
| **Integrations** | `useLiveQuery` for React, over a database in the page or a synced replica · SDKs for Python, JavaScript, Go and .NET · embedded in Swift, Kotlin and Dart/Flutter apps, live queries as SwiftUI observables, Flows and Streams · LangChain and LlamaIndex vector stores, each passing its framework's own tests |
| **Access** | a server token · HS256 and RS256 JSON Web Tokens (JWKS, rotated by `kid`) held to a policy, down to the rows (`owner = $jwt.sub`) and the fields each row shows · a refused token waiting longer each time · an audit log of refusals, schema changes and admin actions |
| **Operations** | read replicas and promotion · archives and backups sealed with a key, restored to a moment, every tenant of a node over one connection · a file per tenant behind a router, failed over on a lease |
| **Regions** | nodes in regions and zones, a tenant's replica in another zone of its region · a region keeping its own directory, leased and failed over by its own router while cut off ([its own directory](https://fenecdb.com/docs/sharding#region-directories)) · one URL for every region: a Cloudflare Worker that learns each tenant's home from the routers and names each client to them under an HMAC, so a refused token waits by the client's address ([global entry](https://fenecdb.com/docs/sharding#global-entry)) · read replicas in other regions: the reads a client marks, through any SDK (`reads: 'nearest'` in JavaScript), answered near it and never from before its own write, a subscription sent after that write waiting for it too, and a JavaScript live query's runs after each change its stream at home tells of ([read replicas](https://fenecdb.com/docs/sharding#read-replicas)) · a kit to run them: systemd units, alert rules, a runbook and a failover drill ([deploy/](deploy/RUNBOOK.md)) |
| **Monitoring** | `/_metrics` for Prometheus — statements and their latency per transport, data, replication — a Grafana dashboard, `--slow-ms`, and OpenTelemetry traces over OTLP (`--otlp-endpoint`) |
| **Studio** | `fenec-server --studio`: collections, rows, edits, a query editor, the schema, live rows and the server's numbers in a browser at `/_studio/`, with the token you paste and no authority of its own |
| **Runtime size** | 301 KB gzip wasm + 59 KB gzip client, or the client alone (`@fenecdb/web/client`) · 1331–1897 KB binary · 2.99 MB container image |

Full reference: [FenecQL](https://fenecdb.com/docs/fenecql).

What it deliberately does **not** do — no second writer (a batch holds the
database from its first write to its end), no interactive transactions, no
SQL dialect (SQL's everyday spellings are read as FenecQL) and no PostgreSQL
wire protocol, no JOIN (an uncorrelated `in (get ...)` aside),
no change of a field's type in place, no multi-writer replication, no decimal
type, no TLS (a terminator goes in front) — is listed
with its reasoning in [Limits](https://fenecdb.com/docs/limits), alongside every
ceiling baked into the code. Relations are `lookup`, which attaches a
collection's matching documents to the row they belong to with a `limit` that
counts children per parent, and chains — `products → reviews → authors` is one
query; it is not a join and is not trying to be one.

---

## Documentation

| | |
|---|---|
| [Quickstart](https://fenecdb.com/docs/quickstart) | Build it, open a file, write a row and query it |
| [How it works](https://fenecdb.com/docs/concepts) | Segments, the offset index, the mapped file, why there is no page cache and exactly one writer, and HNSW with a filter |
| [FenecQL](https://fenecdb.com/docs/fenecql) | Statements, types, indexes, operators, parameters, functions |
| [From SQL](https://fenecdb.com/docs/sql) | The SQL FenecQL reads as written, what it refuses and what to write instead, where the meaning is not SQL's: nulls, double quotes, text |
| [JavaScript client](https://fenecdb.com/docs/javascript) | The browser client, the immutable query builder, binding it to a transport, live queries as an app's state, a schema declared in code |
| [Mobile and native apps](https://fenecdb.com/docs/mobile) | Swift, Kotlin and Dart/Flutter: a file on the device, the builder, live queries driving the UI, syncing with a server, TLS and the background, where the file lives, durability |
| [HTTP endpoint](https://fenecdb.com/docs/http) | REST/JSON derived from the schema, raw FenecQL, batches, SSE, vector search over POST, a read marked for a replica |
| [Server](https://fenecdb.com/docs/server) | Running `fenec-server`: flags, durability, tokens and policies, the audit log, limits, containers |
| [Replication](https://fenecdb.com/docs/replication) | Read replicas fed the writes on the primary's disk, promotion by hand, what a failover loses, backups and restoring to a moment, every tenant of a node archived over one connection |
| [Integrations](https://fenecdb.com/docs/integrations) | `useLiveQuery` for React, the Go and .NET SDKs, and LangChain and LlamaIndex vector stores over HTTP |
| [Agents and MCP](https://fenecdb.com/docs/mcp) | An agent's search and memory over the Model Context Protocol: `fenec-server --mcp` and `fenec mcp` for Claude Code, Cursor and Claude Desktop, each tool held to the token's rules, answers past a bound refused rather than cut |
| [Monitoring](https://fenecdb.com/docs/monitoring) | `/_metrics` in Prometheus's format, the Grafana dashboard in `monitoring/`, the slow-statement log and a request id in every line |
| [Datadog and others](https://fenecdb.com/docs/datadog) | The Datadog Agent's check, logs pipeline and dashboard in `monitoring/datadog/`; New Relic, Elastic and Grafana Cloud |
| [Grafana](https://fenecdb.com/docs/grafana) | The server's metrics through Prometheus and the database's own rows through `fenecdb-datasource` (`integrations/grafana`), each panel FenecQL with the time range and variables bound as parameters, alert rules included, under a read-only token; or through the Infinity plugin; the dashboards in `monitoring/` |
| [Sync](https://fenecdb.com/docs/sync) | A local replica that reads without the network and writes optimistically |
| [Instead of Redis](https://fenecdb.com/docs/redis) | The patterns an app adds Redis for, each with its recipe — counters, rate limits, locks with fencing tokens, sessions, caches, leaderboards, sets, unique visitors, delayed jobs, streams, pub/sub, idempotency keys — and what fenecdb does not do that Redis does |
| [Job queues](https://fenecdb.com/docs/queues) | Claims under a lease, acks, retries with a backoff, dead letters, delays and priorities, and a claim held at the server until a job comes |
| [Points and distances](https://fenecdb.com/docs/geo) | A `geo` field: distances in metres, rows within a radius or a box, the nearest first, positions that expire |
| [Analytics and market data](https://fenecdb.com/docs/analytics) | Rollups by the minute, distinct users, funnels and retention over events; bars, VWAP and the latest price over ticks |
| [Tenants and sharding](https://fenecdb.com/docs/sharding) | A file per tenant, many per node, and a router that places, moves and fails them over; regions and zones, a region that keeps its own directory, one URL for every region, and read replicas near the client |
| [Deploying across regions](deploy/RUNBOOK.md) | `deploy/`: systemd units for nodes, routers, archivers and the tunnel, alert rules, the operator's runbook, and `make drill`, which kills a node and a router of a two-region deployment in Docker and checks no acknowledged write was lost |
| [Import](https://fenecdb.com/docs/import) | Build a collection from SQLite or a live PostgreSQL server in one command, and keep it following the table's changes |
| [Embedded Rust](https://fenecdb.com/docs/embedding) | `fenec-core` as a library: opening a file, executing parsed statements |
| [File format](https://fenecdb.com/docs/file-format) | One file, replayed in a single pass; record kinds, and the crate layout |
| [Benchmarks](https://fenecdb.com/docs/benchmarks) | YCSB against SQLite, PostgreSQL, MongoDB and Redis, writers beside readers, a request over HTTP and text search; then vectors, against SQLite and pgvector in process and fenec-server against pgvector at scale |
| [Limits](https://fenecdb.com/docs/limits) | What it does not do, every hard-coded ceiling, memory and scale |

The docs are the long-form reference; their source is
[`site/content/docs/`](site/content/docs/), so a correction is a pull request
like any other.

Coding agents get the same docs as text, rendered from those pages on every
build: [`llms.txt`](https://fenecdb.com/llms.txt) is a brief with the rules
FenecQL does not share with SQL, and
[`llms-full.txt`](https://fenecdb.com/llms-full.txt) is every page in one file.

---

## Contributing

`CONTRIBUTING.md` has the setup, the narrower test invocations and the
invariants a change must not break.

## License

Apache-2.0. See `LICENSE`.
