# Notes -- Node and TypeScript (server)

A notes CLI over a `fenec-server`, from Node, through
`@fenecdb/web/client`: the query builder and the HTTP client alone. No
WebAssembly is fetched or loaded; every query runs on the server.

## What it shows

- the schema declared in code, Drizzle's way (`src/tables.ts`, `fenecTable`
  from `@fenecdb/web/schema`), brought to the server by
  `connect(url, { schema, migrate: true })`, and four notes seeded into an
  empty collection;
- every query typed by the table: a misspelt field is a compile error;
- creating notes, and marking one done;
- full-text search with `match`, fused with `near` over a toy embedding
  (`fuse`);
- filtering by tag (`tags has`) or by `done`, newest first;
- `watch`: a live query over the server -- `db.live(query, cb)` runs the
  query there again after every write to `notes`;
- `smoke` checks that no request was for a `.wasm`.

## Prerequisites

Node 22 or newer, and `fenec-server`: a [release
binary](https://github.com/fenecdb/fenec/releases), or
`cargo build --release -p fenec-server` at the repository's root
(`target/release/fenec-server`).

## Run

```sh
fenec-server --file notes.fenec --http 127.0.0.1:8080 --http-token secret
```

```sh
cd examples/node-server
npm install
npm start                                # seeds, then lists
npm start -- add "Dentist" "Call the dentist on Monday." health
npm start -- list --tag home
npm start -- list --open
npm start -- search istanbul trip        # match, then fuse
npm start -- done 1
npm start -- watch                       # Ctrl-C to stop; add a note from another terminal
npm run smoke                            # type-checks, then the whole tour, checked: what CI runs
```

`FENEC_URL` and `FENEC_TOKEN` point it at another server; `secret` is a
development default, never one to deploy.

## The core

```ts
import { connect } from '@fenecdb/web/client';   // no engine, no .wasm
import { notes, embed } from './tables.js';     // fenecTable('notes', { ... })

const db = await connect(url, { token, schema: { notes }, migrate: true });
await db.from(notes).insert({ title, body, tags: ['home'], done: false, at: new Date(), embed: embed(`${title} ${body}`) });

const open = db.from(notes).where('done', false).order('at', 'desc');
db.live(open, (rows) => show(rows));            // again after every write to notes
await db.from(notes).match('body', words).near('embed', embed(words)).fuse().limit(5).rows();
await db.from(notes).where('id', id).update({ done: true });
```

## The toy embedding

`embed()` in `src/tables.ts` is a placeholder, not a model: character
trigrams (of the UTF-8 bytes) hashed into 64 dimensions with FNV-1a, so
`near` and `fuse` have something to rank without a download. It matches
spelling, not meaning; every Notes example computes the same vectors. A
real app calls a model here -- an embeddings API, or transformers.js's
`pipeline('feature-extraction', ...)` -- and declares
`vector({ dimensions: N })` with that model's `N`.

## Against this repository's build

CI installs `@fenecdb/web` packed from `web/` in this checkout rather than
npm's: `examples/run-tests.sh node-server` copies the folder, points the
dependency at the tarball (`npm pkg set dependencies.@fenecdb/web=file:...`),
installs, and runs `npm run smoke` against a `fenec-server` it builds and
starts.
