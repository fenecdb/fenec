# fenecdb for Python

A client for fenec-pg's HTTP endpoint (`fenec-pg --http <address>`), and
vector stores for LangChain and LlamaIndex on top of it.

```sh
pip install "fenecdb[langchain]"      # or "fenecdb[llama-index]"
```

```python
from fenecdb import Client

db = Client("http://127.0.0.1:8080", token="...")
db.query("get articles select title near embed $1 limit 5", [[0.1, 0.2, 0.3]])
```

In an event loop -- FastAPI, aiohttp, an agent -- `AsyncClient` makes the
same calls awaited, over one kept-alive connection, the standard library
alone:

```python
from fenecdb import AsyncClient

async with AsyncClient("http://127.0.0.1:8080", token="...") as db:
    rows = await db.query("get articles select title near embed $1 limit 5", [[0.1, 0.2, 0.3]])
    await db.batch([("put articles {title: $1}", ["a"]), ("put articles {title: $1}", ["b"])])
```

Every write on the server's disk (`fenec-pg --cdc`), for a consumer whose
cursor the server keeps -- each batch committed once the loop comes back
for the next, so each write comes at least once -- and the same handed on
to another program or a webhook:

```python
for batch in db.follow("search-index"):
    for change in batch:               # {"seq", "at", "collection", "op", "id", "doc"}
        index(change)
```

```sh
python -m fenecdb.relay http://127.0.0.1:8080 --consumer kafka | kcat -P -b broker:9092 -t fenec
```

```python
from fenecdb.langchain import FenecVectorStore

store = FenecVectorStore(embeddings, "docs", url="http://127.0.0.1:8080", token="...",
                         metadata_fields={"source": "text"})
store.add_documents(documents)
store.similarity_search("how do I compact", k=4, filter={"source": "handbook"})
```

```python
from fenecdb.llama_index import FenecVectorStore
from llama_index.core import StorageContext, VectorStoreIndex

store = FenecVectorStore("docs", url="http://127.0.0.1:8080", token="...")
index = VectorStoreIndex.from_documents(
    documents, storage_context=StorageContext.from_defaults(vector_store=store)
)
```

A document or node is a row -- its id, text, metadata as JSON and vector
under `@hnsw` -- in a collection created on the first write. Fields named in
`metadata_fields` get columns of their own under `@hash`, and filters over
them are answered by the index before the search runs.

With `full_text=True` the text is indexed for BM25 too, and both stores search
by the words alone or by the words and the vector fused -- LangChain's
`mode="text"` and `mode="hybrid"`, LlamaIndex's `TEXT_SEARCH` and `HYBRID`.
`quant="int8"` or `"bit"` keeps the graph over codes.

```python
store = FenecVectorStore(embeddings, "docs", url=..., token=..., full_text=True)
store.similarity_search("how do I compact", k=4, mode="hybrid")
```

`./run-tests.sh` runs LangChain's standard vector store suite and the tests
LlamaIndex's own integrations run against a fenec-pg it builds and starts.
Full reference: https://fenecdb.com/docs/integrations
