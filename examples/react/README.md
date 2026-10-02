# Notes -- React (local, or synced by one line)

A notes app in React whose list is `useLiveQuery`. It runs over a database
in the page; one line in `src/db.ts` makes it a replica synced with a
`fenec-server` instead, and the components do not change.

## What it shows

- the schema declared in code (`src/tables.ts`, `fenecTable` from
  `@fenecdb/web/schema`), every query typed by it;
- `useLiveQuery(db.from(notes)...)`: the rows, rendered again after every
  write -- this tab's, or, synced, another client's streamed from the
  server;
- creating notes, and marking one done with a click;
- a search box: `match`, fused with `near` over a toy embedding (`fuse`);
- filtering by tag (`tags has`) or by `done`, newest first;
- local: `restore` at the open and `persist` after every write
  (IndexedDB); synced: `sync({ url, shapes, schema })`, which reads from the
  page and sends the writes.

## Prerequisites

Node 22 or newer. For the synced line, `fenec-server`: a [release
binary](https://github.com/fenecdb/fenec/releases), or
`cargo build --release -p fenec-server` at the repository's root.

## Run

```sh
cd examples/react
npm install
npm run dev                              # http://localhost:5173, a database in the page
npm run build                            # type-checks, then dist/
npm run smoke                            # both ways, headless in Node: what CI runs (needs the server below)
```

To sync with a server, start one that lets the page's origin in:

```sh
fenec-server --file notes.fenec --http 127.0.0.1:8080 --http-token secret \
  --http-cors http://localhost:5173
```

and switch the line in `src/db.ts`:

```ts
// export const db = await local(wasm);
export const db = await synced(SERVER, TOKEN, wasm);
```

Open the page in two browsers: a note added in one shows in the other.
`VITE_FENEC_URL` and `VITE_FENEC_TOKEN` point it at another server. The
`secret` default is for development: the server's own token can change its
schema, and a real app hands each user a JWT scoped to their rows
([Sync](https://fenecdb.com/docs/sync)).

## The core

```tsx
// src/db.ts: the one line
export const db = await local(wasm);                      // Fenec.open(wasm, { schema: { notes } })
// export const db = await synced(SERVER, TOKEN, wasm);   // sync({ url, schema, shapes: [{ collection: notes }] })

// src/App.tsx: the same either way
const rows = useLiveQuery(db.from(notes).where('done', false).order('at', 'desc').limit(20));
const hits = useLiveQuery(db.from(notes).match('body', words).near('embed', embed(words)).fuse().limit(20));
await db.from(notes).insert({ title, body, tags, done: false, at: new Date(), embed: embed(`${title} ${body}`) });
await db.from(notes).where('id', n.id).update({ done: true });
```

## The toy embedding

`embed()` in `src/tables.ts` is a placeholder, not a model: character
trigrams (of the UTF-8 bytes) hashed into 64 dimensions with FNV-1a, so
`near` and `fuse` have something to rank without a download. It matches
spelling, not meaning; every Notes example computes the same vectors. In a
real app, transformers.js computes them in the page
(`pipeline('feature-extraction', 'Xenova/all-MiniLM-L6-v2')`, 384
dimensions), or a server's embeddings API does; declare
`vector({ dimensions: N })` with that model's `N`.

## Against this repository's build

CI installs `@fenecdb/web` and `@fenecdb/react` packed from this checkout
rather than npm's: `examples/run-tests.sh react` copies the folder, points
both dependencies at the tarballs, installs, runs `npm run smoke` against a
`fenec-server` it starts and `npm run build`, then switches the line in
`src/db.ts` and type-checks the components again.
