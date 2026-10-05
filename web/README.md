# fenecdb for the browser

The engine as WebAssembly, and the client that drives it: `@fenecdb/web` on
npm, and the `fenec-web` bundle of each release.

| File | What it is |
| --- | --- |
| `fenec.js` | the client: the module's glue, persistence and the sync layer, a dependency-free ES module that imports and re-exports the two below |
| `builder.js`, `http.js` | the query builder and the HTTP client |
| `client.js` | the two of them, and nothing that could pull in the engine, sync or storage: `@fenecdb/web/client`, 7 KB brotli in an app's bundle |
| `fenec.d.ts`, `client.d.ts` | their types |
| `schema.js`, `schema.d.ts` | a schema declared in code, Drizzle's way: `@fenecdb/web/schema` |
| `fenec.wasm` | the engine with every index and the schema check: 185 KB brotli |
| `collate/` | the collation data the module fetches beside it, a chunk a group of scripts |

```js
import { Fenec } from './fenec.js';
const db = await Fenec.open('./fenec.wasm');
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

**Which package.** A page whose queries run on a server imports
`@fenecdb/web/client` and loads no module; a database in the page, or a
synced replica, loads `fenec.wasm`, with every index and the schema check.
A page that needs fewer indexes can build its own module with them alone --
`make wasm FEATURES="text sorted"`, `FEATURES=none` for none, and `SCHEMA=0`
leaves the schema check out (7.5 KB brotli) -- and the file is the same
either way: a store one build wrote opens in the other. Without the graph a
`near` measures every vector, as `exact` does; without the text index a
`match` throws a `FenecError` naming it.

Serve `.wasm` as `application/wasm`, compressed once at build time.
Full reference: https://fenecdb.com/docs/javascript
