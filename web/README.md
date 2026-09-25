# fenecdb for the browser

The engine as WebAssembly, and the client that drives it: `@fenecdb/web` on
npm, and the `fenec-web` bundle of each release.

| File | What it is |
| --- | --- |
| `fenec.js` | the client: the module's glue, the query builder, the HTTP client and the sync layer, one dependency-free ES module |
| `fenec.d.ts` | its types |
| `fenec.wasm` | the engine with every index: 150 KB brotli |
| `fenec-lite.wasm` | the engine without its four indexes: 120 KB brotli |
| `collate/` | the collation data the module fetches beside it, a chunk a group of scripts |

```js
import { Fenec } from './fenec.js';
const db = await Fenec.open('./fenec.wasm');      // or './fenec-lite.wasm'
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
layer, and none of the four indexes: no graph behind `near` and `fuse`, no
text index behind `match` and `rerank`, no sparse index behind a
`sparse<N>` field's `near`, no ordered index (the scan answers a `@sorted`
field's comparisons and orders, with the same rows). A page that uses none
of them saves 30 KB brotli with it. A statement that needs a missing index
throws a `FenecError` naming it, and the file is the same either way: a
store one module wrote opens in the other. A page that needs some of the
indexes builds its module with them alone: `make wasm FEATURES="text sorted"`.

Serve `.wasm` as `application/wasm`, compressed once at build time.
Full reference: https://fenecdb.com/docs/javascript
