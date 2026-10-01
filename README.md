# fenecdb

[![ci](https://github.com/fenecdb/fenec/actions/workflows/ci.yml/badge.svg)](https://github.com/fenecdb/fenec/actions/workflows/ci.yml)

An embedded document database with full-text and vector search built in.
Documents, indexes, aggregates, transactions, BM25 and HNSW in one engine,
written in Rust with no dependencies. It runs inside a web page as 192 KB of
gzipped WebAssembly, in a Rust process, or as a server that speaks the
PostgreSQL protocol, and it has its own query language (**FenecQL**).

**[fenecdb.com](https://fenecdb.com)** — the website and documentation. The
home page boots the real WebAssembly module and builds an HNSW index in your
browser, and races fenec-pg against pgvector over a million vectors.
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
worth making — fenecdb is not aiming at that job. That is exactly why `fenec-pg`
exists: not to *replace* PostgreSQL but to reach fenecdb with the same tools.

The numbers behind the comparison, and the method that produced them, are in
[Benchmarks](https://fenecdb.com/docs/benchmarks) -- with `fenec-pg` against
PostgreSQL and pgvector over the same wire, at a million vectors
([At scale](https://fenecdb.com/docs/benchmarks#scale)).

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

192 KB of gzipped WebAssembly and a 30 KB gzipped client — no wasm-bindgen, no
build step — and smaller built without the four indexes for a page that uses
none of them (`make wasm FEATURES=none`, or any set of them). [JavaScript client](https://fenecdb.com/docs/javascript).

**PostgreSQL server.** `fenec-pg` answers psql, psycopg, asyncpg, pgx,
tokio-postgres, node-postgres, Npgsql, JDBC, PHP's PDO and Ruby's pg
([drivers](https://fenecdb.com/docs/postgres#drivers)), and the catalog they
look around in: `\d`, JDBC's `DatabaseMetaData` and
DBeaver's navigator see the collections, their fields and their indexes.
A vector is pgvector's `vector`, `halfvec` or `sparsevec`, so pgvector's
client libraries for Python, Go, Node, Rust, .NET, Java, PHP and Ruby work unchanged
([pgvector's clients](https://fenecdb.com/docs/postgres#pgvector)).
`COPY ... FROM STDIN` loads rows as psql's `\copy` and psycopg's `copy` send
them, in text, CSV or binary ([COPY](https://fenecdb.com/docs/postgres#copy)).

```bash
make pg PGPASS=secret HTTP=127.0.0.1:8080
psql -h 127.0.0.1 -p 5432 -U fenec
```

The same process carries the HTTP/JSON endpoint — never a second binary, since
two processes opening one file would corrupt it. A second `fenec-pg` follows
it as a read replica with `--replica-of`: it is sent the writes on the
primary's disk, serves reads, refuses writes with `25006`, and is promoted by
hand; `fenec backup`, `fenec archive` and `fenec restore --to <time>` take a
running database whole, keep its writes, and rebuild it as it stood at a
moment. With `--follow postgres://...` it mirrors a PostgreSQL table into its
file as the table commits, and serves the mirror while it follows. [PostgreSQL server](https://fenecdb.com/docs/postgres) ·
[HTTP endpoint](https://fenecdb.com/docs/http) ·
[Replication](https://fenecdb.com/docs/replication).

**Container.** 2.88 MB, and the `Dockerfile` is two-stage: static musl build
into `scratch`, so the runtime image holds the binary and nothing else — no
shell, no package manager, no libc.

```bash
docker pull ghcr.io/fenecdb/fenec-pg:0.1.6     # published, multi-arch
make docker && make docker-run PGPASS=secret   # or build it yourself
```

**Embedded in Rust.** `fenec-core` is the engine as a library:
[Embedded Rust](https://fenecdb.com/docs/embedding).

---

## What it does

| | |
|---|---|
| **Documents** | `insert` (refuses a taken id), `put` (upsert), `set` and `del` by filter, a batch or a transaction landing whole |
| **Schema** | `alter collection` adds, drops and renames a field without rewriting a document — and over the pg wire, the `ALTER TABLE` a migration sends |
| **Transactions** | `BEGIN`, `SAVEPOINT`, `ROLLBACK TO`, `COMMIT` over the PostgreSQL wire; `COPY` in and out |
| **Types** | `bool` `int` `float` `text` `bytes` `timestamp` `vector<N[, f16]>` `sparse<N>` `[type]` `json` (objects, lists and scalars; a path such as `meta.source.rank` reads into it in `where`, `select`, `order` and `set`, and `jsonb` over the pg wire) |
| **Indexes** | `@hash`, `@unique` (a second document holding a value refused, `null` aside), `@sorted`, `@hnsw(metric, m=.., ef_construction=.., ef_search=.., quant=int8\|bit)`, `@text(k1=.., b=.., prefix=..)`, `@inverted`; `@hash`, `@unique` and `@sorted` on a path into a `json` field too |
| **Metrics** | `cosine` `l2` `dot` |
| **Operators** | `= != < <= > >=`, `~` (text contains, case-insensitive), `has` (list contains), `in [..]`, `is null` |
| **Retrieval** | `near` (HNSW; exact by dot product over a `sparse<N>` such as SPLADE's), `match` (BM25), `rerank` (exact vector reordering of `match` candidates, no graph needed), `fuse` (`match` and `near` ranking together, by reciprocal rank) |
| **Aggregates** | `count(*)` `sum` `avg` `min` `max`, whole or per `group`, ordered and paged by any of them |
| **Collation** | `order name collate und` — Unicode's order for every script, as ICU's root orders it (PostgreSQL's `und-x-icu`); `collate tr` Turkish (`ç` after `c`, `ı` before `i`, `tr-x-icu`); a field declared in one pages by its last row; bytes otherwise |
| **Relations** | `lookup` — a collection's matching documents attached per row, `limit` counted per parent, chainable to 8 levels |
| **Functions** | `lower upper len coalesce now timestamp cosine l2 dot norm normalize` + plugins |
| **Interfaces** | FenecQL · a JS query builder · REST/JSON + SSE · PostgreSQL v3 wire · WASM C ABI · change data capture (`/_changes`, every write on disk as a JSON line, resumable) |
| **Integrations** | LangChain and LlamaIndex vector stores, each passing its framework's own tests · `useLiveQuery` for React |
| **Access** | SCRAM passwords and a read-only user · a server token · HS256 and RS256 JSON Web Tokens (JWKS, rotated by `kid`) held to a policy, down to the rows (`owner = $jwt.sub`) · an audit log of logins, refusals and schema changes |
| **Operations** | read replicas and promotion · archives and backups sealed with a key, restored to a moment · a file per tenant behind a router, failed over on a lease |
| **Monitoring** | `/_metrics` for Prometheus — statements and their latency per transport, data, replication — a Grafana dashboard, and `--slow-ms` |
| **Runtime size** | 192 KB gzip wasm + 30 KB gzip client · 1153–1687 KB binary · 2.88 MB container image |

Full reference: [FenecQL](https://fenecdb.com/docs/fenecql).

What it deliberately does **not** do — no second writer (a transaction holds the
database from its first write to its end), no JOIN, no subqueries,
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
| [JavaScript client](https://fenecdb.com/docs/javascript) | The browser client, the immutable query builder, binding it to a transport |
| [HTTP endpoint](https://fenecdb.com/docs/http) | REST/JSON derived from the schema, vector search over POST, raw FenecQL, SSE |
| [PostgreSQL server](https://fenecdb.com/docs/postgres) | Sessions, SCRAM authentication, the type mapping, what the protocol does not carry |
| [Replication](https://fenecdb.com/docs/replication) | Read replicas fed the writes on the primary's disk, promotion by hand, what a failover loses, backups and restoring to a moment |
| [Integrations](https://fenecdb.com/docs/integrations) | LangChain and LlamaIndex vector stores over HTTP, `useLiveQuery` for React |
| [Monitoring](https://fenecdb.com/docs/monitoring) | `/_metrics` in Prometheus's format, the Grafana dashboard in `monitoring/`, and the slow-statement log |
| [Sync](https://fenecdb.com/docs/sync) | A local replica that reads without the network and writes optimistically |
| [Tenants and sharding](https://fenecdb.com/docs/sharding) | A file per tenant, many per node, and a router that places and moves them |
| [Import](https://fenecdb.com/docs/import) | Build a collection from SQLite or a live PostgreSQL server in one command, and keep it following the table's changes |
| [Embedded Rust](https://fenecdb.com/docs/embedding) | `fenec-core` as a library: opening a file, executing parsed statements |
| [File format](https://fenecdb.com/docs/file-format) | One file, replayed in a single pass; record kinds, and the crate layout |
| [Benchmarks](https://fenecdb.com/docs/benchmarks) | Against SQLite and pgvector on the same data in the same process, and against pgvector over the pg wire at scale |
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
