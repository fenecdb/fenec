# fenecdb for the browser

The engine as WebAssembly, and the client that drives it: `@fenecdb/web` on
npm, and the `fenec-web` bundle of each release.

| File | What it is |
| --- | --- |
| `fenec.js` | the client: the module's glue, persistence and the sync layer, a dependency-free ES module that imports and re-exports the two below |
| `builder.js`, `http.js` | the query builder and the HTTP client |
| `client.js` | the two of them, and nothing that could pull in the engine, sync or storage: `@fenecdb/web/client`, 6 KB brotli in an app's bundle |
| `fenec.d.ts`, `client.d.ts` | their types |
| `schema.js`, `schema.d.ts` | a schema declared in code, Drizzle's way: `@fenecdb/web/schema` |
| `fenec.wasm` | the engine with every index and the schema check: 174 KB brotli |
| `fenec-replica.wasm` | for a replica that searches no vectors or holds a few thousand, opt-in (`sync({ wasm })`): no graph -- `near` measures every vector, as `exact` does -- and no schema check: 147 KB brotli |
| `fenec-lite.wasm` | the engine without its four indexes or the schema check: 131 KB brotli |
| `collate/` | the collation data the module fetches beside it, a chunk a group of scripts |

```js
import { Fenec } from './fenec.js';
const db = await Fenec.open('./fenec.wasm');      // or './fenec-lite.wasm'
```

A page whose queries run on a server never fetches the module -- only
`Fenec.open` does -- and can import the client entry, which holds nothing of
the engine, the sync layer or storage (8 KB brotli bundled through
`@fenecdb/web`, 6 through it):

```js
import { connect } from '@fenecdb/web/client';   // or './client.js', beside builder.js and http.js
const db = connect('https://db.example.com', { token });
const rows = await db.from('docs').where('year', '>=', 2024).limit(10).rows();
const stop = db.live(db.from('docs').where('done', false), render);   // again after a write it may read
```

**From npm** the files are the same. A page serves the module and `collate/`
as static files and hands `Fenec.open` the module's URL -- with Vite, say:

```js
import { Fenec } from '@fenecdb/web';
import wasm from '@fenecdb/web/fenec.wasm?url';
const db = await Fenec.open(wasm, { collation: '/collate/' });   // collate/ copied into public/
```

Node reads both out of the package:

```js
import { readFile } from 'node:fs/promises';
import { Fenec } from '@fenecdb/web';
const file = (path) => readFile(new URL(import.meta.resolve(`@fenecdb/web/${path}`)));
const db = await Fenec.open(await file('fenec.wasm'), {
  collation: (name) => file(`collate/${name}.bin`),
});
```

**Which module.** `fenec-lite.wasm` has documents, filters, `order`,
aggregates, `lookup`, collations, the change feed, persistence and the sync
layer, and none of the four indexes: no graph behind `near` and `fuse` --
they measure every vector, as `exact` does, the same rows and scores -- no
text index behind `match` and `rerank`, no sparse index behind a
`sparse<N>` field's `near` -- it scores every document -- no ordered index
(the scan answers a `@sorted` field's comparisons and orders, with the same
rows), and no check of a schema declared in code (`Fenec.open`'s `schema`
is refused). A page that uses none of them saves 43 KB brotli with it.
`fenec-replica.wasm` leaves out the graph and the schema check alone; `sync()`
opens the full module unless given `wasm: './fenec-replica.wasm'`, since a
`near` without the graph takes 0.54 ms over 1 000 x 128 against 0.27, 8.5 ms
over 10 000 x 384 against 0.68 and 40 over 50 000 x 384 against 1.0. A `match` or a `create index` that needs a missing index throws a
`FenecError` naming it, and the file is the same either way: a store one
module wrote opens in the others. A page that needs some of the
indexes builds its module with them alone: `make wasm FEATURES="text sorted"`,
and `SCHEMA=0` leaves the schema check out of it (7.5 KB brotli).

Serve `.wasm` as `application/wasm`, compressed once at build time.
Full reference: https://fenecdb.com/docs/javascript
