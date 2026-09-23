"""A LangChain vector store over fenecdb's HTTP endpoint.

    from fenecdb.langchain import FenecVectorStore

    store = FenecVectorStore(embeddings, "docs", url="http://127.0.0.1:8080", token="...")
    store.add_documents(documents)
    store.similarity_search("how do I compact", k=4)

A document is a row: its id, its text, its metadata as JSON, and its vector
under `@hnsw`. The collection is created on the first write, when the
embedding's size is known. A metadata field named in `metadata_fields` also
gets a column of its own, under `@hash`, and `filter` takes equality over
those -- `filter={"source": "handbook"}` is `where source = $2`, answered by
the index, before `near` looks for neighbours among what passed.

Writing an id that is already there replaces it: the old row out and the new
one in, sent as one batch and run under one write lock.
"""

from __future__ import annotations

import json
import re
import uuid
from typing import Any, Callable, Iterable, Sequence

from langchain_core.documents import Document
from langchain_core.embeddings import Embeddings
from langchain_core.vectorstores import VectorStore

from fenecdb import Client, FenecError, placeholders

__all__ = ["FenecVectorStore"]

# Names go into the statement's text, so they are held to FenecQL's.
_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
_TYPES = {"text", "int", "float", "bool", "timestamp"}
_RESERVED = {"id", "lc_id", "content", "metadata", "embedding"}
_METRICS = {"cosine", "l2", "dot"}


class FenecVectorStore(VectorStore):
    """A collection in fenecdb, holding LangChain documents."""

    def __init__(
        self,
        embedding: Embeddings,
        collection: str = "langchain",
        *,
        url: str = "http://127.0.0.1:8080",
        token: str | None = None,
        client: Client | None = None,
        metric: str = "cosine",
        metadata_fields: dict[str, str] | None = None,
    ):
        if not _NAME.match(collection):
            raise ValueError(f"not a collection name: {collection!r}")
        if metric not in _METRICS:
            raise ValueError(f"metric is one of {sorted(_METRICS)}, not {metric!r}")
        fields = dict(metadata_fields or {})
        for name, ty in fields.items():
            if not _NAME.match(name) or name in _RESERVED:
                raise ValueError(f"not a metadata field this store can index: {name!r}")
            if ty not in _TYPES:
                raise ValueError(f"{name}: a field is one of {sorted(_TYPES)}, not {ty!r}")
        self._embedding = embedding
        self.collection = collection
        self._client = client or Client(url, token)
        self._metric = metric
        self._fields = fields
        self._dimension: int | None = None

    @property
    def embeddings(self) -> Embeddings:
        return self._embedding

    # ------------------------------------------------------------ writing

    def add_texts(
        self,
        texts: Iterable[str],
        metadatas: list[dict] | None = None,
        *,
        ids: list[str | None] | None = None,
        **kwargs: Any,
    ) -> list[str]:
        texts = list(texts)
        if not texts:
            return []
        metadatas = list(metadatas) if metadatas else [{} for _ in texts]
        # A document that came without an id gets one, and so does each of
        # the documents without one among some that have theirs.
        ids = [i or str(uuid.uuid4()) for i in (ids or [None] * len(texts))]
        # numpy's floats are not JSON's.
        vectors = [[float(x) for x in v] for v in self._embedding.embed_documents(texts)]
        self._ensure(len(vectors[0]))

        docs, params = [], []
        for doc_id, text, meta, vector in zip(ids, texts, metadatas, vectors):
            values = [doc_id, text, json.dumps(meta, default=str), vector]
            values += [meta.get(f) for f in self._fields]
            names = ["lc_id", "content", "metadata", "embedding", *self._fields]
            first = len(params) + 1
            docs.append(
                "{" + ", ".join(f"{n}: ${first + i}" for i, n in enumerate(names)) + "}"
            )
            params += values
        self._client.batch(
            [
                (f"del {self.collection} where lc_id in [{placeholders(1, len(ids))}]", ids),
                (f"put {self.collection} [{', '.join(docs)}]", params),
            ]
        )
        return ids

    def delete(self, ids: list[str] | None = None, **kwargs: Any) -> bool | None:
        """Deletes these ids, or every document when `ids` is None."""
        if ids is None:
            statement, params = f"del {self.collection}", []
        else:
            params = list(ids)
            if not params:
                return True
            statement = (
                f"del {self.collection} where lc_id in [{placeholders(1, len(params))}]"
            )
        self._run(statement, params)
        return True

    def drop_collection(self) -> None:
        """Drops the collection, documents and index together."""
        self._client.query(f"drop collection if exists {self.collection}")
        self._dimension = None

    # ------------------------------------------------------------ reading

    def get_by_ids(self, ids: Sequence[str], /) -> list[Document]:
        ids = list(ids)
        if not ids:
            return []
        rows = self._run(
            f"get {self.collection} select lc_id, content, metadata "
            f"where lc_id in [{placeholders(1, len(ids))}]",
            ids,
        )
        return [self._document(r) for r in rows]

    def similarity_search(
        self, query: str, k: int = 4, filter: dict | None = None, **kwargs: Any
    ) -> list[Document]:
        return [d for d, _ in self.similarity_search_with_score(query, k, filter, **kwargs)]

    def similarity_search_with_score(
        self, query: str, k: int = 4, filter: dict | None = None, **kwargs: Any
    ) -> list[tuple[Document, float]]:
        """Documents with fenecdb's score: the cosine similarity under
        `cosine`, higher is nearer; the distance under `l2`; the product
        under `dot`."""
        vector = self._embedding.embed_query(query)
        return self.similarity_search_with_score_by_vector(vector, k, filter, **kwargs)

    def similarity_search_by_vector(
        self, embedding: list[float], k: int = 4, filter: dict | None = None, **kwargs: Any
    ) -> list[Document]:
        pairs = self.similarity_search_with_score_by_vector(embedding, k, filter, **kwargs)
        return [d for d, _ in pairs]

    def similarity_search_with_score_by_vector(
        self, embedding: list[float], k: int = 4, filter: dict | None = None, **kwargs: Any
    ) -> list[tuple[Document, float]]:
        where, params = self._where(filter, first=2)
        rows = self._run(
            f"get {self.collection} select lc_id, content, metadata{where} "
            f"near embedding $1 limit {int(k)}",
            [[float(x) for x in embedding], *params],
        )
        return [(self._document(r), r["_score"]) for r in rows]

    def _select_relevance_score_fn(self) -> Callable[[float], float]:
        # Under `cosine` the score already is 1 - distance, LangChain's
        # relevance; under `dot` the product is, for normalised vectors.
        if self._metric == "l2":
            return self._euclidean_relevance_score_fn
        return lambda score: score

    @classmethod
    def from_texts(
        cls,
        texts: list[str],
        embedding: Embeddings,
        metadatas: list[dict] | None = None,
        *,
        ids: list[str] | None = None,
        **kwargs: Any,
    ) -> "FenecVectorStore":
        store = cls(embedding, **kwargs)
        store.add_texts(texts, metadatas, ids=ids)
        return store

    # ------------------------------------------------------------ inside

    def _ensure(self, dimension: int) -> None:
        if self._dimension == dimension:
            return
        extra = "".join(f", {f} {t} @hash" for f, t in self._fields.items())
        self._client.query(
            f"create collection if not exists {self.collection} ("
            f"lc_id text @hash, content text, metadata text, "
            f"embedding vector<{dimension}> @hnsw({self._metric}){extra})"
        )
        self._dimension = dimension

    def _run(self, statement: str, params: list) -> Any:
        """A statement over a collection that may not be there yet: a store
        nothing was written to is an empty one, not an error."""
        try:
            return self._client.query(statement, params)
        except FenecError as e:
            if e.status == 404 and str(e).startswith("collection `"):
                return []
            raise

    def _where(self, filter: dict | None, first: int) -> tuple[str, list]:
        """` where f = $n and g in [$m, ...]` over the indexed fields."""
        if not filter:
            return "", []
        terms, params = [], []
        for name, want in filter.items():
            if name not in self._fields:
                raise ValueError(
                    f"`{name}` is not in metadata_fields: only those can be filtered on"
                )
            if isinstance(want, dict):
                if set(want) == {"$eq"}:
                    want = want["$eq"]
                elif set(want) == {"$in"}:
                    want = list(want["$in"])
                else:
                    raise ValueError(f"{name}: only $eq and $in are supported, not {want}")
            if isinstance(want, (list, tuple, set)):
                values = list(want)
                n = first + len(params)
                terms.append(f"{name} in [{placeholders(n, len(values))}]")
                params += values
            else:
                terms.append(f"{name} = ${first + len(params)}")
                params.append(want)
        return " where " + " and ".join(terms), params

    @staticmethod
    def _document(row: dict) -> Document:
        return Document(
            id=row["lc_id"],
            page_content=row["content"],
            metadata=json.loads(row["metadata"] or "{}"),
        )
