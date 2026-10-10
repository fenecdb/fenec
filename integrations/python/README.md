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
```

Aggregates go in `select` as FenecQL spells them, and `group` takes one key
or more; `bucket`, `count_distinct`, `first`, `last` and `expr` make a
column, named with `.as_(name)` (`as` being Python's keyword):

```python
from fenecdb import bucket, expr, first, last

bars = (db.collection("ticks")
          .select(bucket("at", "1m").as_("minute"), first("px", "at").as_("open"),
                  "max(px)", "min(px)", last("px", "at").as_("close"),
                  expr("sum(px * qty) / sum(qty)").as_("vwap"))
          .where("sym", "S07")
          .group("minute")
          .order("minute", "desc")
          .rows())
```

`facet(field, ranges=[0, 25, 50])` counts by ranges of numbers, each
value `[from, to]`, and `disjunctive=True` counts past the filter's own
condition on the field, as a shop's filter list does.

Several statements are one `batch`, a block under one write lock: all
land or none, and a `FenecError` that stops it says which (`e.at`, from 0;
412 for a write whose `require` was not met). A retry after a timeout
cannot tell whether a write ran, so a write that must not run twice goes
under an idempotency key -- the second answer is the first one kept
(`db.replayed`), and the key with another request is refused (422):

```python
from fenecdb import inc

debit = docs.where("id", 1).to_update({"stock": inc(-1)}, require=1)
db.batch([debit, ("put orders {item: $1}", [1])], idempotency_key=order_id)
db.with_idempotency_key(order_id).collection("orders").insert({"item": 1})
db.seq                                        # the change the last write left the database at
```

A job queue's claim is one statement: `order` and `limit` before
`update` pick the rows it writes, and `returning=True` (or a list of
fields) answers them as written, where a count is answered otherwise:

```python
from fenecdb import expr, inc, raw

claimed = (
    db.collection("jobs").where(raw("run_at <= now()")).order("run_at").limit(10)
    .update({"owner": me, "run_at": expr("now() + ?", 30000), "attempts": inc(1)}, returning=True)
)
```

To see the text and parameters a chain builds, for logging or a test,
`to_fenecql()` returns them and runs nothing:
`docs.select("title").limit(5).to_fenecql()` is
`("get articles select title limit 5", [])`. A query needs no call to it
before it runs.

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
