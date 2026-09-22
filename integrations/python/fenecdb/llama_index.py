"""A LlamaIndex vector store over fenecdb's HTTP endpoint.

    from fenecdb.llama_index import FenecVectorStore
    from llama_index.core import StorageContext, VectorStoreIndex

    store = FenecVectorStore("docs", url="http://127.0.0.1:8080", token="...")
    index = VectorStoreIndex.from_documents(
        documents, storage_context=StorageContext.from_defaults(vector_store=store)
    )

A node is a row: its id, its document's id, its text, its metadata as
LlamaIndex serialises it, and its vector under `@hnsw`. The collection is
created on the first write, when the embedding's size is known. A metadata
field named in `metadata_fields` also gets a column of its own, under `@hash`,
and a query's filters are answered over those -- `==`, `!=`, `<`, `<=`, `>`,
`>=`, `in` and `nin`, joined by `and` or `or` -- before `near` looks for
neighbours among what passed.
"""

from __future__ import annotations

import json
import re
from typing import Any, List, Optional, Sequence

from llama_index.core.bridge.pydantic import PrivateAttr
from llama_index.core.schema import BaseNode, MetadataMode
from llama_index.core.vector_stores.types import (
    BasePydanticVectorStore,
    FilterCondition,
    FilterOperator,
    MetadataFilter,
    MetadataFilters,
    VectorStoreQuery,
    VectorStoreQueryMode,
    VectorStoreQueryResult,
)
from llama_index.core.vector_stores.utils import (
    metadata_dict_to_node,
    node_to_metadata_dict,
)

from fenecdb import Client, FenecError, placeholders

__all__ = ["FenecVectorStore"]

_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
_TYPES = {"text", "int", "float", "bool", "timestamp"}
_RESERVED = {"id", "node_id", "ref_doc_id", "content", "metadata", "embedding"}
_METRICS = {"cosine", "l2", "dot"}
_COMPARE = {
    FilterOperator.EQ: "=",
    FilterOperator.NE: "!=",
    FilterOperator.GT: ">",
    FilterOperator.GTE: ">=",
    FilterOperator.LT: "<",
    FilterOperator.LTE: "<=",
}


class FenecVectorStore(BasePydanticVectorStore):
    """A collection in fenecdb, holding LlamaIndex nodes."""

    stores_text: bool = True
    flat_metadata: bool = False

    collection: str = "llama_index"
    url: str = "http://127.0.0.1:8080"
    token: Optional[str] = None
    metric: str = "cosine"
    metadata_fields: dict = {}

    _client: Client = PrivateAttr()
    _dimension: Optional[int] = PrivateAttr(default=None)

    def __init__(
        self,
        collection: str = "llama_index",
        url: str = "http://127.0.0.1:8080",
        token: Optional[str] = None,
        metric: str = "cosine",
        metadata_fields: Optional[dict] = None,
        client: Optional[Client] = None,
        **kwargs: Any,
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
        super().__init__(
            collection=collection,
            url=url,
            token=token,
            metric=metric,
            metadata_fields=fields,
            **kwargs,
        )
        self._client = client or Client(url, token)

    @classmethod
    def class_name(cls) -> str:
        return "FenecVectorStore"

    @property
    def client(self) -> Client:
        return self._client

    # ------------------------------------------------------------ writing

    def add(self, nodes: Sequence[BaseNode], **kwargs: Any) -> List[str]:
        """Writes the nodes; a node id already there is replaced, the old
        row out and the new one in as one batch under one write lock."""
        nodes = list(nodes)
        if not nodes:
            return []
        ids = [n.node_id for n in nodes]
        vectors = [[float(x) for x in n.get_embedding()] for n in nodes]
        self._ensure(len(vectors[0]))
        names = ["node_id", "ref_doc_id", "content", "metadata", "embedding"]
        names += list(self.metadata_fields)
        docs, params = [], []
        for node, vector in zip(nodes, vectors):
            meta = node_to_metadata_dict(
                node, remove_text=True, flat_metadata=self.flat_metadata
            )
            values = [
                node.node_id,
                node.ref_doc_id,
                node.get_content(metadata_mode=MetadataMode.NONE),
                json.dumps(meta, default=str),
                vector,
            ]
            values += [node.metadata.get(f) for f in self.metadata_fields]
            first = len(params) + 1
            docs.append(
                "{" + ", ".join(f"{n}: ${first + i}" for i, n in enumerate(names)) + "}"
            )
            params += values
        c = self.collection
        self._client.batch(
            [
                (f"del {c} where node_id in [{placeholders(1, len(ids))}]", ids),
                (f"put {c} [{', '.join(docs)}]", params),
            ]
        )
        return ids

    def delete(self, ref_doc_id: str, **delete_kwargs: Any) -> None:
        """Deletes every node of a document."""
        self._run(f"del {self.collection} where ref_doc_id = $1", [ref_doc_id])

    def delete_nodes(
        self,
        node_ids: Optional[List[str]] = None,
        filters: Optional[MetadataFilters] = None,
        **delete_kwargs: Any,
    ) -> None:
        """Deletes these nodes, or those the filters pass. Given neither it
        deletes nothing: everything is `clear`, and asked for by name."""
        if node_ids is None and filters is None:
            return
        where, params = self._where(node_ids, None, filters, first=1)
        self._run(f"del {self.collection}{where}", params)

    def clear(self) -> None:
        self._run(f"del {self.collection}", [])

    def drop_collection(self) -> None:
        """Drops the collection, nodes and index together."""
        self._client.query(f"drop collection if exists {self.collection}")
        self._dimension = None

    # ------------------------------------------------------------ reading

    def get_nodes(
        self,
        node_ids: Optional[List[str]] = None,
        filters: Optional[MetadataFilters] = None,
    ) -> List[BaseNode]:
        where, params = self._where(node_ids, None, filters, first=1)
        rows = self._run(
            f"get {self.collection} select node_id, content, metadata{where}", params
        )
        return [self._node(r) for r in rows]

    def query(self, query: VectorStoreQuery, **kwargs: Any) -> VectorStoreQueryResult:
        if query.mode != VectorStoreQueryMode.DEFAULT:
            raise NotImplementedError(f"query mode {query.mode} is not supported")
        if query.query_embedding is None:
            raise ValueError("a query embedding is needed")
        # An empty list is no restriction here, as LlamaIndex means it: its
        # retriever sends `node_ids=[]` for a store that keeps its own text.
        where, params = self._where(
            query.node_ids or None, query.doc_ids or None, query.filters, first=2
        )
        rows = self._run(
            f"get {self.collection} select node_id, content, metadata{where} "
            f"near embedding $1 limit {int(query.similarity_top_k)}",
            [[float(x) for x in query.query_embedding], *params],
        )
        return VectorStoreQueryResult(
            nodes=[self._node(r) for r in rows],
            similarities=[r["_score"] for r in rows],
            ids=[r["node_id"] for r in rows],
        )

    # ------------------------------------------------------------ inside

    def _ensure(self, dimension: int) -> None:
        if self._dimension == dimension:
            return
        extra = "".join(f", {f} {t} @hash" for f, t in self.metadata_fields.items())
        self._client.query(
            f"create collection if not exists {self.collection} ("
            f"node_id text @hash, ref_doc_id text @hash, content text, metadata text, "
            f"embedding vector<{dimension}> @hnsw({self.metric}){extra})"
        )
        self._dimension = dimension

    def _run(self, statement: str, params: list) -> Any:
        """A collection nothing was written to yet is an empty one."""
        try:
            return self._client.query(statement, params)
        except FenecError as e:
            if e.status == 404 and str(e).startswith("collection `"):
                return []
            raise

    def _where(
        self,
        node_ids: Optional[List[str]],
        doc_ids: Optional[List[str]],
        filters: Optional[MetadataFilters],
        first: int,
    ) -> tuple[str, list]:
        terms, params = [], []
        for field, values in (("node_id", node_ids), ("ref_doc_id", doc_ids)):
            if values is not None:
                n = first + len(params)
                terms.append(f"{field} in [{placeholders(n, len(values))}]")
                params += list(values)
        if filters is not None and filters.filters:
            terms.append(self._filters(filters, params, first))
        if not terms:
            return "", []
        return " where " + " and ".join(terms), params

    def _filters(self, filters: MetadataFilters, params: list, first: int) -> str:
        joiner = {FilterCondition.AND: " and ", FilterCondition.OR: " or "}.get(
            filters.condition
        )
        if joiner is None:
            raise NotImplementedError(f"filter condition {filters.condition} is not supported")
        parts = []
        for f in filters.filters:
            if isinstance(f, MetadataFilters):
                parts.append(self._filters(f, params, first))
            else:
                parts.append(self._filter(f, params, first))
        return "(" + joiner.join(parts) + ")"

    def _filter(self, f: MetadataFilter, params: list, first: int) -> str:
        if f.key not in self.metadata_fields:
            raise ValueError(
                f"`{f.key}` is not in metadata_fields: only those can be filtered on"
            )
        if f.operator in _COMPARE:
            params.append(f.value)
            return f"{f.key} {_COMPARE[f.operator]} ${first + len(params) - 1}"
        if f.operator in (FilterOperator.IN, FilterOperator.NIN):
            values = list(f.value)
            n = first + len(params)
            params += values
            term = f"{f.key} in [{placeholders(n, len(values))}]"
            return term if f.operator == FilterOperator.IN else f"not ({term})"
        raise NotImplementedError(f"filter operator {f.operator} is not supported")

    @staticmethod
    def _node(row: dict) -> BaseNode:
        node = metadata_dict_to_node(json.loads(row["metadata"] or "{}"))
        node.set_content(row["content"] or "")
        return node
