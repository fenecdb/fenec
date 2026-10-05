# Examples

One app, **Notes**, in every language fenecdb speaks, so they can be read
side by side. Each folder is a small working app with a README: what it
shows, what it needs, the commands to run it, and the ten lines that matter.

Every example has the same collection,

```
create collection notes (
  title text,
  body  text @text,
  tags  [text],
  done  bool @hash,
  at    timestamp @sorted,
  embed vector<64> @hnsw(cosine)
)
```

declared in code: `fenecTable` from `@fenecdb/web/schema` in TypeScript, a
`schema.fenecql` handed to the SDK's `schema(...)` everywhere else. Each
creates notes, searches them with `match`, filters them by tag or by
`done`, orders them by date, and ranks them by `near` and `fuse` over the
same four seeded notes. Where the platform has one, a live query redraws
the list after every change; a local engine keeps its notes across
restarts.

| Example | Language | Kind | What it shows | Run |
| --- | --- | --- | --- | --- |
| [`web-local`](web-local) | TypeScript, Vite | local | the engine in the page, kept in IndexedDB; a live list and a search box | `npm install && npm run dev` |
| [`react`](react) | React, TypeScript | local, or synced | `useLiveQuery`; one line in `src/db.ts` switches the page's database for a replica `sync()`ed with a server | `npm install && npm run dev` |
| [`node-server`](node-server) | Node, TypeScript | server | `@fenecdb/web/client`: the builder over HTTP and live queries on the server, no WebAssembly | `npm install && npm start` |
| [`python`](python) | Python | server | the `fenecdb` client and its builder; the change stream as `watch`; a LangChain store | `python notes.py` |
| [`go`](go) | Go | server | the Go SDK, rows into structs, a subscription | `go mod tidy && go run .` |
| [`dotnet`](dotnet) | C#, .NET 8 | server | `FenecDb`, rows into records, `SubscribeAsync` | `dotnet run` |
| [`rust`](rust) | Rust | local | `fenec-core` embedded in a CLI, FenecQL with parameters | `cargo run` |
| [`swift`](swift) | Swift, SwiftUI | local | a macOS app (the same views on iOS) over a file in Application Support, `LiveQuery`; a CLI | `swift run NotesApp` |
| [`kotlin-android`](kotlin-android) | Kotlin, Compose | local | an Android app over a file in `filesDir`, a live `Flow`; a JVM CLI | `gradle :app:installDebug` |
| [`flutter`](flutter) | Dart, Flutter | local | a Flutter app over a file in Application Support, `StreamBuilder` over a live query | `flutter run` |
| [`shop`](shop) | Next.js, TypeScript | server | not the Notes app: a desert-gear shop of 10 000 products -- category pages with facets, search with highlights, carts under scoped tokens, a checkout that cannot oversell -- with its tests, Lighthouse scores and load test | `npm install && npm run db`, then `npm run seed && npm run build && npm start` |
| [`ledger`](ledger) | Node, TypeScript | server | not the Notes app: a double-entry payments ledger on a tenant node -- transfers, refunds and holds as guarded `/batch` blocks with idempotency keys, an append-only journal, reconciliation, the journal streamed to a sink, scoped and tenant-bound tokens -- held by 16 clients' 20 000 operations, a `kill -9` under load and its security tests, an operator console | `npm install && npm run db`, then `npm run setup && npm start` |
| [`integrations/cloudflare/example`](../integrations/cloudflare/example) | JavaScript | Durable Object | a database kept in a Durable Object's storage (`@fenecdb/cloudflare`); not the Notes app | `npx wrangler dev` |

*Server* means the example talks to a `fenec-server` over HTTP; start one
first:

```sh
fenec-server --file notes.fenec --http 127.0.0.1:8080 --http-token secret
```

A release binary is on the [releases page](https://github.com/fenecdb/fenec/releases),
or `cargo build --release -p fenec-server` builds one here. Every example
reads `FENEC_URL` and `FENEC_TOKEN`; `secret` is a development default,
never one to deploy.

## The toy embedding

`near` and `fuse` need vectors, and a real model is a download no example
should start with. So each example computes a **toy embedding**: character
trigrams (of the UTF-8 bytes) hashed with FNV-1a into 64 dimensions,
normalised. It matches spelling, not meaning -- "istanbul trip" finds the
note about flights to Istanbul because they share letters -- and every
example computes the same vectors to the bit (each smoke test checks
`embed("hello")`). A real app puts a model where `embed` is: transformers.js
in a page, an embeddings API on a server, Core ML or ONNX on a device -- and
declares `vector<N>` with that model's `N`.

## Versions, and this repository's build

Each example depends on the published packages at the release's version
(`make version` moves them with every other package). CI runs every one on
what this checkout builds instead, each its own way: npm packs of `web/`
and `integrations/react`, `PYTHONPATH`, a Go `replace`, a .NET project
reference (`-p:FenecLocal`), Cargo paths, SwiftPM's local package
(`FENEC_LOCAL=1`), a Gradle composite build (`FENEC_LOCAL=1`) and pub's
overrides.

```sh
make examples-test                        # every example whose toolchain is here
examples/run-tests.sh python go rust      # some of them
SHOP_LIGHTHOUSE=1 examples/run-tests.sh shop   # the shop, its Lighthouse budgets too
```

Each runs its `smoke`: create, search, filter, a live update where the
platform has one, a reopen where the engine is local. The SwiftUI and
Compose apps and the Flutter app are built in CI (on macOS, and on Ubuntu
with the Android SDK); their headless parts run there and here.
