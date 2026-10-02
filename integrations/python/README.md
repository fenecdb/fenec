# fenecdb for Python

A client for fenec-server's HTTP endpoint (`fenec-server --http <address>`), and
vector stores for LangChain and LlamaIndex on top of it.

```sh
pip install "fenecdb[langchain]"      # or "fenecdb[llama-index]"
```

```python
from fenecdb import Client

db = Client("http://127.0.0.1:8080", token="...")
db.query("get articles select title near embed $1 limit 5", [[0.1, 0.2, 0.3]])
```

Or the query builder writes the statement -- every value a parameter,
every name checked -- the same text the JavaScript, Go and .NET builders
make of the same chain:

```python
from fenecdb import or_

docs = db.collection("articles")
rows = (docs.select("title")
            .where("year", ">=", 2024)
            .where(or_({"lang": "tr"}, {"tags": {"has": "rust"}}))
            .near("embed", [0.1, 0.2, 0.3], ef=64)
            .limit(5)
            .rows())
docs.insert({"title": "Dunes", "year": 2021})
docs.where("year", "<", 2000).delete()        # no filter: refused unless all=True
docs.where("lang", "tr").count()
docs.select("title").limit(5).to_fenecql()   # ("get articles select title limit 5", [])
```

In an event loop -- FastAPI, aiohttp, an agent -- `AsyncClient` makes the
same calls awaited, over one kept-alive connection, the standard library
alone:

```python
from fenecdb import AsyncClient

async with AsyncClient("http://127.0.0.1:8080", token="...") as db:
    rows = await db.query("get articles select title near embed $1 limit 5", [[0.1, 0.2, 0.3]])
    await db.batch([("put articles {title: $1}", ["a"]), ("put articles {title: $1}", ["b"])])
    rows = await db.collection("articles").where("year", ">=", 2024).rows()
```

Every write on the server's disk (`fenec-server --cdc`), for a consumer whose
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
LlamaIndex's own integrations run against a fenec-server it builds and starts,
and the query builder against every case of `integrations/builder-golden.json`.
Full reference: https://fenecdb.com/docs/integrations
