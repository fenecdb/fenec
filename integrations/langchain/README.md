# @fenecdb/langchain

A LangChain.js vector store over fenecdb: the database in the page (a
`Fenec` from `@fenecdb/web`), or behind fenec-server's HTTP endpoint (a
`FenecHttp`) -- anything with `run(sql, params)`.

```js
import { Fenec } from '@fenecdb/web';
import { FenecVectorStore } from '@fenecdb/langchain';

const db = await Fenec.open('./fenec.wasm');
const store = new FenecVectorStore(embeddings, {
  client: db,
  collection: 'handbook',
  metadataFields: { source: 'text' },
  fullText: true,
});
await store.addDocuments(documents);
await store.similaritySearch('how do I compact', 4, { source: 'handbook' });
await store.search('compact', 4, { mode: 'hybrid' });
```

It is the Python store's collection (`pip install "fenecdb[langchain]"`),
from JavaScript:

- A document is a row: its id, text, metadata as JSON and vector under
  `@hnsw`, over int8 or bit codes with `quant`. The collection is made on
  the first write, once the embedding's size is known.
- A field named in `metadataFields` gets a column of its own under `@hash`.
  `filter` takes equality and `$in` over those, answered by the index
  before the search ranks what passed.
- With `fullText`, the text is indexed for BM25 as well.
  `search(query, k, {mode})` ranks by the words (`'text'`) or by the words
  and the vector fused (`'hybrid'`).
- Writing an id that is already there replaces it. On a `Fenec` both
  statements run as one block; over HTTP they are two requests.

`npm test` holds it to what LangChain.js's own vector store integrations
are tested for, over a database in the page and over a fenec-server. It needs
`web/fenec.wasm` and the fenec-server binary.
