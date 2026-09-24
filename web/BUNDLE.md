# fenecdb for the browser

What a page needs, as the release's `fenec-web` bundle ships it:

| File | What it is |
| --- | --- |
| `fenec.js` | the client: the module's glue, the query builder, the HTTP client and the sync layer, one dependency-free ES module |
| `fenec.d.ts` | its types |
| `fenec.wasm` | the engine with every index: 145 KB brotli |
| `fenec-lite.wasm` | the engine without its four indexes: 117 KB brotli |
| `collate/` | the collation data the module fetches beside it, a chunk a group of scripts |

```js
import { Fenec } from './fenec.js';
const db = await Fenec.open('./fenec.wasm');      // or './fenec-lite.wasm'
```

**Which module.** `fenec-lite.wasm` has documents, filters, `order`,
aggregates, `lookup`, collations, the change feed, persistence and the sync
layer, and none of the four indexes: no graph behind `near` and `fuse`, no
text index behind `match` and `rerank`, no sparse index behind a
`sparse<N>` field's `near`, no ordered index (the scan answers a `@sorted`
field's comparisons and orders, with the same rows). A page that uses none
of them saves 27 KB brotli with it. A statement that needs a missing index
throws a `FenecError` naming it, and the file is the same either way: a
store one module wrote opens in the other. A page that needs some of the
indexes builds its module with them alone: `make wasm FEATURES="text sorted"`.

Serve `.wasm` as `application/wasm`, compressed once at build time.
Full reference: https://fenecdb.com/docs/javascript
