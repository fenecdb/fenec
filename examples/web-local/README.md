# Notes -- the browser (local)

A notes page whose database runs in the page: `@fenecdb/web`'s engine as
WebAssembly, kept in IndexedDB between visits. No server.

## What it shows

- the schema declared in code, Drizzle's way (`src/tables.ts`, `fenecTable`
  from `@fenecdb/web/schema`): `Fenec.open(wasm, { schema })` makes it, and
  checks it at every open;
- `restore` at the open and `persist` after each write: only the writes
  since go into IndexedDB, not the whole image;
- creating notes, and marking one done with a click;
- a search box: `match`, fused with `near` over a toy embedding (`fuse`);
- filtering by tag (`tags has`) or by `done`, newest first;
- the list is a live query: `db.live(query, draw)` draws it again after
  every write to `notes`.

## Prerequisites

Node 22 or newer for Vite. The page runs in any current browser.

## Run

```sh
cd examples/web-local
npm install
npm run dev                              # http://localhost:5173
npm run build                            # type-checks, then dist/: static files, the .wasm among them
npm run smoke                            # the page's logic in Node, headless: what CI runs
```

Reload the page: the notes are where you left them.

## The core

```ts
import wasm from '@fenecdb/web/fenec.wasm?url';
import { Fenec, persist, restore } from '@fenecdb/web';
import { notes, embed } from './tables.js';         // fenecTable('notes', { ... })

const db = await Fenec.open(wasm, { schema: { notes } });
await restore(db, 'notes');                          // last visit's database, checked against the schema

db.live(db.from(notes).where('done', false).order('at', 'desc'), draw);   // now, and after every write
await db.from(notes).insert({ title, body, tags, done: false, at: new Date(), embed: embed(`${title} ${body}`) });
await persist(db, 'notes');                          // only this write
db.from(notes).match('body', words).near('embed', embed(words)).fuse().limit(20);
```

## The toy embedding

`embed()` in `src/tables.ts` is a placeholder, not a model: character
trigrams (of the UTF-8 bytes) hashed into 64 dimensions with FNV-1a, so
`near` and `fuse` have something to rank without a download. It matches
spelling, not meaning; every Notes example computes the same vectors. In a
real page, transformers.js computes them in the page
(`pipeline('feature-extraction', 'Xenova/all-MiniLM-L6-v2')`, 384
dimensions), or a server's embeddings API does; declare
`vector({ dimensions: N })` with that model's `N`.

## Against this repository's build

CI installs `@fenecdb/web` packed from `web/` in this checkout (after `make
wasm`) rather than npm's: `examples/run-tests.sh web-local` copies the
folder, points the dependency at the tarball (`npm pkg set
dependencies.@fenecdb/web=file:...`), installs, and runs `npm run smoke` and
`npm run build`.
