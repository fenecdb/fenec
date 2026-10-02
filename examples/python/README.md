# Notes -- Python (server)

A notes CLI over a `fenec-server`, with the `fenecdb` client: the standard
library alone, every query sent over HTTP.

## What it shows

- the schema in `schema.fenecql`, applied with `db.schema(..., migrate=True)`
  at every start, and four notes seeded into an empty collection;
- creating notes through the query builder, and marking one done;
- full-text search with `match`, fused with `near` over a toy embedding
  (`fuse`);
- filtering by tag (`tags has`) or by `done`, newest first (`order at desc`);
- `watch`: the open notes printed again after every change. The Python
  client has no live queries; the server's change stream (`--cdc`) stands
  in for one.

## Prerequisites

Python 3.10 or newer, and `fenec-server`: a [release
binary](https://github.com/fenecdb/fenec/releases), or
`cargo build --release -p fenec-server` at the repository's root
(`target/release/fenec-server`).

## Run

```sh
fenec-server --file notes.fenec --http 127.0.0.1:8080 --http-token secret --cdc
```

```sh
cd examples/python
pip install -r requirements.txt
python notes.py                          # seeds, then lists
python notes.py add "Dentist" "Call the dentist on Monday." health
python notes.py list --tag home
python notes.py list --open
python notes.py search istanbul trip     # match, then fuse
python notes.py done 1
python notes.py watch                    # Ctrl-C to stop; add a note from another terminal
python notes.py smoke                    # the whole tour, checked: what CI runs
```

`FENEC_URL` and `FENEC_TOKEN` point it at another server; `secret` is a
development default, never one to deploy.

## The core

```python
db = Client(URL, token=TOKEN)
db.schema(open("schema.fenecql").read(), migrate=True)   # made, or checked
notes = db.collection("notes")
notes.insert({"title": t, "body": b, "tags": ["home"], "done": False,
              "at": datetime.now(timezone.utc).isoformat(), "embed": embed(f"{t} {b}")})

notes.where("tags", "has", "work").order("at", "desc").rows()
notes.where("done", False).order("at", "desc").rows()
notes.match("body", words).rows()
notes.match("body", words).near("embed", embed(words)).fuse().limit(5).rows()
notes.where("id", 3).update({"done": True})
```

## LangChain

The same package holds a LangChain vector store over a collection of its
own (`pip install "fenecdb[langchain]"`):

```python
from fenecdb.langchain import FenecVectorStore

store = FenecVectorStore(embeddings, "docs", url=URL, token=TOKEN, full_text=True)
store.add_texts(["Find cheap flights to Istanbul for the conference in spring."])
store.similarity_search("trip to Istanbul", k=4, mode="hybrid")   # match and near, fused
```

`embeddings` is any LangChain `Embeddings`; LlamaIndex has a store too
([Integrations](https://fenecdb.com/docs/integrations)).

## The toy embedding

`embed()` is a placeholder, not a model: character trigrams (of the UTF-8
bytes) hashed into 64 dimensions, so `near` and `fuse` have something to
rank without a download. It matches spelling, not meaning; every Notes
example computes the same vectors. A real app calls a model here -- an
embeddings API, or `sentence-transformers` locally -- and declares
`vector<N>` with that model's `N` in `schema.fenecql`.

## Against this repository's build

CI runs the example on the client in this checkout rather than PyPI's:

```sh
cd examples/python
PYTHONPATH=../../integrations/python python notes.py smoke
```

`examples/run-tests.sh python` does that against a `fenec-server` it builds
and starts.
