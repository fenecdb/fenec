# fenecdb

[![ci](https://github.com/fenecdb/fenec/actions/workflows/ci.yml/badge.svg)](https://github.com/fenecdb/fenec/actions/workflows/ci.yml)

An embedded document database with full-text and vector search built in.
Documents, indexes, aggregates, atomic batches, BM25 and HNSW in one engine,
written in Rust with no dependencies. It runs inside a web page as 234 KB of
gzipped WebAssembly, in an iOS, Android or Flutter app as a file on the
device, in a Rust process, or as a server any language reaches over HTTP,
and it has its own query language (**FenecQL**).

**[fenecdb.com](https://fenecdb.com)** — the website and documentation. The
home page boots the real WebAssembly module and builds an HNSW index in your
browser, and races fenec-server, over HTTP, against pgvector over a million
vectors.
Source in [`site/`](site/): `make site-serve` runs it locally, `make site-deploy`
publishes it to Cloudflare Workers.

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

Lexical recall and an exact vector reordering on top of it — the second stage
reads the stored vectors, so this path needs no HNSW graph at all:

```
get articles select title
  match body $1
  rerank embed $2 candidates 500
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

Where the words matched, and how many results each brand has — for a
results page and its filter sidebar, from the same query:

```
get products select name, highlight(name, "<mark>", "</mark>")
  match name $1
  where price < 500
  facet brand top 10, color
  limit 20
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

## Which one, when

**fenecdb** makes sense when the database should run beside the code that uses
it — in the browser, in the process, or as one small server — and search
belongs in it rather than in a second system: an app that keeps its data
offline, local and semantic search, RAG and agent memory, a service with a
file per tenant. The scale limit is **the file image fitting in memory**: not
just the vector arena, all of the records are in memory
([Limits](https://fenecdb.com/docs/limits)).

**SQLite** when you need relational data, SQL with JOINs, several writers to
one file and a mature toolchain. Also in very short-lived processes: for a tool that runs
fewer than five queries and exits, fenecdb's open cost never amortises.

**PostgreSQL + pgvector** for multi-writer systems shared over a network that
need ACID. On maturity, concurrency and ecosystem the comparison is not even
worth making — fenecdb is not aiming at that job. A table can move the
other way, though: `fenec import` copies one from PostgreSQL, and `--follow`
keeps it following the table's commits.

The numbers behind the comparison, and the method that produced them, are in
[Benchmarks](https://fenecdb.com/docs/benchmarks) -- with `fenec-server` over
HTTP against PostgreSQL and pgvector over its own protocol, at a million
vectors ([At scale](https://fenecdb.com/docs/benchmarks#scale)). On YCSB's
workloads in process, fenecdb does B with 16 threads at 545 k operations a
second against SQLite's 47.9 k, and C on one thread at 438 k against
279 k, its file compacted on its own as updates grow it ([YCSB](https://fenecdb.com/docs/benchmarks#ycsb)).

---

## Install

```bash
git clone https://github.com/fenecdb/fenec && cd fenec
make test     # the Rust suite, then the JS suite
make serve    # builds the wasm and serves the browser console on :8787
```

Five minutes from clone to a vector query:
**[Quickstart](https://fenecdb.com/docs/quickstart)**.

**[Examples](examples/)**: one small app, Notes, in every language -- the
browser, React, Node, Python, Go, .NET, Rust, SwiftUI, Jetpack Compose and
Flutter -- each a folder to open and run, with full-text and vector search,
filters and a live list.

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

234 KB of gzipped WebAssembly and a 49 KB gzipped client — no wasm-bindgen, no
build step. An app whose queries run on a server can import
`@fenecdb/web/client`, 7 KB brotli in its bundle, which cannot pull in the
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
stood at a moment. With `--follow postgres://...` it mirrors a PostgreSQL
table into its file as the table commits, and serves the mirror while it
follows. [Server](https://fenecdb.com/docs/server) ·
[HTTP endpoint](https://fenecdb.com/docs/http) ·
[Replication](https://fenecdb.com/docs/replication).

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
| **Schema** | `alter collection` adds, drops and renames a field without rewriting a document |
| **Schema in code** | tables declared in TypeScript Drizzle's way, or a `schema.fenecql` in any SDK, checked at every open: what only adds is applied, a drop, a rename or a type change refused until a migration says, run once and recorded; `fenec types` writes TypeScript, Python, Go or C# from a database or a schema file |
| **Expiry** | `seen timestamp @ttl(30m)` — a row gone that long after its time: out of every read at once, deleted by the server's sweeper as ordinary deletes |
| **Atomic batches** | `POST /batch` and the browser's `run` of several statements land whole or not at all; an `Idempotency-Key` makes a write once |
| **Types** | `bool` `int` `float` `text` `bytes` `timestamp` `vector<N[, f16]>` `sparse<N>` `[type]` `json` (objects, lists and scalars; a path such as `meta.source.rank` reads into it in `where`, `select`, `order` and `set`; imported from PostgreSQL's `jsonb`) |
| **Indexes** | `@hash`, `@unique` (a second document holding a value refused, `null` aside), `@sorted`, `@ttl(30m)`, `@hnsw(metric, m=.., ef_construction=.., ef_search=.., quant=int8\|bit)`, `@text(k1=.., b=.., prefix=..)`, `@inverted`; `@hash`, `@unique` and `@sorted` on a path into a `json` field too |
| **Metrics** | `cosine` `l2` `dot` |
| **Operators** | `= != < <= > >=`, `~` (text contains, case-insensitive), `has` (list contains), `in [..]`, `in (get ...)` (another collection's rows, run once, up to 100 000 values), `is null` |
| **Retrieval** | `near` (HNSW; exact by dot product over a `sparse<N>` such as SPLADE's), `match` (BM25), `rerank` (exact vector reordering of `match` candidates, no graph needed), `fuse` (`match` and `near` ranking together, by reciprocal rank) |
| **Aggregates** | `count(*)` `sum` `avg` `min` `max`, whole or per `group`, ordered and paged by any of them |
| **Collation** | `order name collate und` — Unicode's order for every script, as ICU's root orders it (PostgreSQL's `und-x-icu`); `collate tr` Turkish (`ç` after `c`, `ı` before `i`, `tr-x-icu`); a field declared in one pages by its last row; bytes otherwise |
| **Relations** | `lookup` — a collection's matching documents attached per row, `limit` counted per parent, chainable to 8 levels |
| **Functions** | `lower upper len coalesce now timestamp cosine l2 dot norm normalize` + plugins |
| **Interfaces** | FenecQL · a query builder in seven languages · REST/JSON + SSE · WASM C ABI · a native C ABI for apps · change data capture (`/_changes`, every write on disk as a JSON line, resumable) · import from SQLite and PostgreSQL |
| **Integrations** | LangChain and LlamaIndex vector stores, each passing its framework's own tests · `useLiveQuery` for React, over a database in the page or a synced replica · SDKs for Python, JavaScript, Go and .NET · embedded in Swift, Kotlin and Dart/Flutter apps, live queries as SwiftUI observables, Flows and Streams |
| **Access** | SCRAM passwords and a read-only user · a server token · HS256 and RS256 JSON Web Tokens (JWKS, rotated by `kid`) held to a policy, down to the rows (`owner = $jwt.sub`) · an audit log of logins, refusals and schema changes |
| **Operations** | read replicas and promotion · archives and backups sealed with a key, restored to a moment · a file per tenant behind a router, failed over on a lease |
| **Monitoring** | `/_metrics` for Prometheus — statements and their latency per transport, data, replication — a Grafana dashboard, and `--slow-ms` |
| **Runtime size** | 234 KB gzip wasm + 49 KB gzip client, or the client alone (`@fenecdb/web/client`) · 1331–1897 KB binary · 2.99 MB container image |

Full reference: [FenecQL](https://fenecdb.com/docs/fenecql).

What it deliberately does **not** do — no second writer (a batch holds the
database from its first write to its end), no interactive transactions, no
SQL and no PostgreSQL wire protocol, no JOIN (an uncorrelated `in (get ...)` aside),
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
| [Quickstart](https://fenecdb.com/docs/quickstart) | Build it, open a file, write a vector query |
| [How it works](https://fenecdb.com/docs/concepts) | Segments, the offset index, HNSW with a filter, half precision, and why there is no page cache and exactly one writer |
| [FenecQL](https://fenecdb.com/docs/fenecql) | Statements, types, indexes, operators, parameters, functions |
| [JavaScript client](https://fenecdb.com/docs/javascript) | The browser client, the immutable query builder, binding it to a transport, live queries as an app's state, a schema declared in code |
| [Mobile and native apps](https://fenecdb.com/docs/mobile) | Swift, Kotlin and Dart/Flutter: a file on the device, the builder, live queries driving the UI, syncing with a server, TLS and the background, where the file lives, durability |
| [HTTP endpoint](https://fenecdb.com/docs/http) | REST/JSON derived from the schema, vector search over POST, raw FenecQL, SSE |
| [Server](https://fenecdb.com/docs/server) | Running `fenec-server`: flags, durability, tokens and policies, the audit log, limits, containers |
| [Replication](https://fenecdb.com/docs/replication) | Read replicas fed the writes on the primary's disk, promotion by hand, what a failover loses, backups and restoring to a moment |
| [Integrations](https://fenecdb.com/docs/integrations) | LangChain and LlamaIndex vector stores over HTTP, `useLiveQuery` for React, the Go and .NET SDKs |
| [Monitoring](https://fenecdb.com/docs/monitoring) | `/_metrics` in Prometheus's format, the Grafana dashboard in `monitoring/`, and the slow-statement log |
| [Sync](https://fenecdb.com/docs/sync) | A local replica that reads without the network and writes optimistically |
| [Tenants and sharding](https://fenecdb.com/docs/sharding) | A file per tenant, many per node, and a router that places and moves them |
| [Import](https://fenecdb.com/docs/import) | Build a collection from SQLite or a live PostgreSQL server in one command, and keep it following the table's changes |
| [Embedded Rust](https://fenecdb.com/docs/embedding) | `fenec-core` as a library: opening a file, executing parsed statements |
| [File format](https://fenecdb.com/docs/file-format) | One file, replayed in a single pass; record kinds, and the crate layout |
| [Benchmarks](https://fenecdb.com/docs/benchmarks) | Against SQLite and pgvector on the same data in the same process, and fenec-server over HTTP against pgvector at scale |
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
