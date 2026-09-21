# fenecdb

[![ci](https://github.com/fenecdb/fenec/actions/workflows/ci.yml/badge.svg)](https://github.com/fenecdb/fenec/actions/workflows/ci.yml)

Minimal, vector-native, browser-resident embedded database. Written in Rust,
compiles to WebAssembly, has its own query language (**FenecQL**) and speaks the
PostgreSQL protocol.

**[fenecdb.com](https://fenecdb.com)** — the website and documentation. The
home page boots the real WebAssembly module and builds an HNSW index in your
browser, then races the result against the measured SQLite and pgvector numbers.
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

**fenecdb** makes sense when vector search has to be first-class and when the
database should run on the user's machine — especially in the browser: local
semantic search, offline RAG, in-browser agent memory, embedded
recommendation. The scale limit is **the file image fitting in memory**: not
just the vector arena, all of the records are in memory
([Limits](https://fenecdb.com/docs/limits)).

**SQLite** when you need relational data, transactional safety, JOINs and a
mature toolchain. Also in very short-lived processes: for a tool that runs
fewer than five queries and exits, fenecdb's open cost never amortises.

**PostgreSQL + pgvector** for multi-writer systems shared over a network that
need ACID. On maturity, concurrency and ecosystem the comparison is not even
worth making — fenecdb is not aiming at that job. That is exactly why `fenec-pg`
exists: not to *replace* PostgreSQL but to reach fenecdb with the same tools.

The numbers behind the comparison, and the method that produced them, are in
[Benchmarks](https://fenecdb.com/docs/benchmarks).

---

## Install

```bash
git clone https://github.com/fenecdb/fenec && cd fenec
make test     # the Rust suite, then the JS suite
make serve    # builds the wasm and serves the browser console on :8787
```

Five minutes from clone to a vector query:
**[Quickstart](https://fenecdb.com/docs/quickstart)**.

**Shell.** `cargo build --release -p fenec-cli`, then:

```bash
./target/release/fenec data.fenec              # interactive
./target/release/fenec data.fenec -c 'get docs limit 5'
```

**Browser.** One dependency-free ES module and the wasm file beside it:

```js
import { Fenec } from './fenec.js';
const db = await Fenec.open('./fenec.wasm');
```

418 KB of WebAssembly — 137 KB brotli (`-q 11`) over the wire, client included — no
wasm-bindgen, no build step. [JavaScript client](https://fenecdb.com/docs/javascript).

**PostgreSQL server.** `fenec-pg` answers psql, psycopg, JDBC and pgx:

```bash
make pg PGPASS=secret HTTP=127.0.0.1:8080
psql -h 127.0.0.1 -p 5432 -U fenec
```

The same process carries the HTTP/JSON endpoint — never a second binary, since
two processes opening one file would corrupt it.
[PostgreSQL server](https://fenecdb.com/docs/postgres) ·
[HTTP endpoint](https://fenecdb.com/docs/http).

**Container.** 1.31 MB, and the `Dockerfile` is two-stage: static musl build
into `scratch`, so the runtime image holds the binary and nothing else — no
shell, no package manager, no libc.

```bash
docker pull ghcr.io/fenecdb/fenec-pg:0.1.4     # published, multi-arch
make docker && make docker-run PGPASS=secret   # or build it yourself
```

**Embedded in Rust.** `fenec-core` is the engine as a library:
[Embedded Rust](https://fenecdb.com/docs/embedding).

---

## What it does

| | |
|---|---|
| **Types** | `bool` `int` `float` `text` `bytes` `timestamp` `vector<N[, f16]>` `[type]` |
| **Indexes** | `@hash`, `@sorted`, `@hnsw(metric, m=.., ef_construction=.., ef_search=..)`, `@text(k1=.., b=.., prefix=..)` |
| **Metrics** | `cosine` `l2` `dot` |
| **Operators** | `= != < <= > >=`, `~` (text contains, case-insensitive), `has` (list contains), `in [..]`, `is null` |
| **Retrieval** | `near` (HNSW), `match` (BM25), `rerank` (exact vector reordering of `match` candidates, no graph needed) |
| **Relations** | `lookup` — a collection's matching documents attached per row, `limit` counted per parent, chainable to 8 levels |
| **Functions** | `lower upper len coalesce now timestamp cosine l2 dot norm normalize` + plugins |
| **Interfaces** | FenecQL · a JS query builder · REST/JSON + SSE · PostgreSQL v3 wire · WASM C ABI |
| **Runtime size** | 418 KB wasm + 69 KB client (137 KB brotli served) · 684–927 KB binary · 1.31 MB container image |

Full reference: [FenecQL](https://fenecdb.com/docs/fenecql).

What it deliberately does **not** do — no transactions, no JOIN, no subqueries,
no schema migration, no multi-writer replication, no decimal type — is listed
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
| [JavaScript client](https://fenecdb.com/docs/javascript) | The browser client, the immutable query builder, binding it to a transport |
| [HTTP endpoint](https://fenecdb.com/docs/http) | REST/JSON derived from the schema, vector search over POST, raw FenecQL, SSE |
| [PostgreSQL server](https://fenecdb.com/docs/postgres) | Sessions, SCRAM authentication, the type mapping, what the protocol does not carry |
| [Sync](https://fenecdb.com/docs/sync) | A local replica that reads without the network and writes optimistically |
| [Tenants and sharding](https://fenecdb.com/docs/sharding) | A file per tenant, many per node, and a router that places and moves them |
| [Import](https://fenecdb.com/docs/import) | Build a collection from SQLite or a live PostgreSQL server in one command |
| [Embedded Rust](https://fenecdb.com/docs/embedding) | `fenec-core` as a library: opening a file, executing parsed statements |
| [File format](https://fenecdb.com/docs/file-format) | One file, replayed in a single pass; record kinds, and the crate layout |
| [Benchmarks](https://fenecdb.com/docs/benchmarks) | Against SQLite and pgvector on the same data in the same process |
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
