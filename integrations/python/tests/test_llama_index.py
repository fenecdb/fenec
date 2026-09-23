"""The LlamaIndex vector store, through the tests LlamaIndex's own vector
store integrations run (Chroma's and Postgres's among them): the class, a
write and a nearest match, a document's nodes deleted by its id, nodes read
and deleted by id and by filter, filters of every operator it takes, and an
index built and queried on top of it."""

import pytest
from llama_index.core import Document, MockEmbedding, StorageContext, VectorStoreIndex
from llama_index.core.schema import NodeRelationship, RelatedNodeInfo, TextNode
from llama_index.core.vector_stores.types import (
    BasePydanticVectorStore,
    FilterCondition,
    FilterOperator,
    MetadataFilter,
    MetadataFilters,
    VectorStoreQuery,
)

from conftest import TOKEN, URL, fresh
from fenecdb.llama_index import FenecVectorStore


@pytest.fixture()
def store():
    s = FenecVectorStore(
        fresh("li"),
        url=URL,
        token=TOKEN,
        metadata_fields={"author": "text", "theme": "text", "year": "int"},
    )
    try:
        yield s
    finally:
        s.drop_collection()


@pytest.fixture()
def nodes() -> list[TextNode]:
    def node(text, id_, doc, meta, vector):
        return TextNode(
            text=text,
            id_=id_,
            relationships={NodeRelationship.SOURCE: RelatedNodeInfo(node_id=doc)},
            metadata=meta,
            embedding=vector,
        )

    return [
        node("lorem ipsum", "c330d77f", "test-0", {"author": "Stephen King", "theme": "Friendship", "year": 1982}, [1.0, 0.0, 0.0]),
        node("dolor sit amet", "c3d1e1dd", "test-1", {"author": "Stephen King", "theme": "Mafia", "year": 1974}, [0.0, 1.0, 0.0]),
        node("consectetur adipiscing", "c3ew11cd", "test-2", {"author": "Hermann Hesse", "theme": "Mafia", "year": 1927}, [0.0, 0.0, 1.0]),
        node("sed do eiusmod", "0b31ae71", "test-3", {"author": "Orhan Pamuk", "theme": "Friendship", "year": 1998}, [1.0, 1.0, 0.0]),
    ]


def ids(result):
    return [n.node_id for n in result.nodes]


def test_class():
    names = [b.__name__ for b in FenecVectorStore.__mro__]
    assert BasePydanticVectorStore.__name__ in names


def test_add_and_query(store, nodes):
    assert store.add(nodes) == [n.node_id for n in nodes]
    r = store.query(VectorStoreQuery(query_embedding=[1.0, 0.0, 0.0], similarity_top_k=1))
    assert ids(r) == ["c330d77f"]
    assert r.nodes[0].get_content() == "lorem ipsum"
    assert r.nodes[0].metadata["author"] == "Stephen King"
    assert r.nodes[0].ref_doc_id == "test-0"
    assert r.similarities[0] == pytest.approx(1.0, abs=1e-5)


def test_an_empty_store_answers_nothing(store):
    r = store.query(VectorStoreQuery(query_embedding=[1.0, 0.0, 0.0], similarity_top_k=3))
    assert r.nodes == [] and r.ids == []
    assert store.get_nodes(node_ids=["x"]) == []
    store.delete("test-0")
    store.clear()


def test_adding_a_node_again_replaces_it(store, nodes):
    store.add(nodes)
    changed = nodes[0].model_copy()
    changed.text = "lorem ipsum, again"
    store.add([changed])
    got = store.get_nodes(node_ids=["c330d77f"])
    assert [n.get_content() for n in got] == ["lorem ipsum, again"]
    assert len(store.get_nodes(node_ids=[n.node_id for n in nodes])) == 4


def test_delete_by_document(store, nodes):
    store.add(nodes)
    store.delete("test-0")
    r = store.query(VectorStoreQuery(query_embedding=[1.0, 0.0, 0.0], similarity_top_k=4))
    assert "c330d77f" not in ids(r)
    assert len(r.nodes) == 3


def test_get_and_delete_nodes(store, nodes):
    store.add(nodes)
    got = store.get_nodes(node_ids=["c3d1e1dd", "c3ew11cd"])
    assert sorted(n.node_id for n in got) == ["c3d1e1dd", "c3ew11cd"]
    by_author = MetadataFilters(filters=[MetadataFilter(key="author", value="Stephen King")])
    assert sorted(n.node_id for n in store.get_nodes(filters=by_author)) == ["c330d77f", "c3d1e1dd"]
    store.delete_nodes(node_ids=["c330d77f"])
    store.delete_nodes(filters=MetadataFilters(filters=[MetadataFilter(key="theme", value="Mafia")]))
    # Neither ids nor filters: nothing, not everything.
    store.delete_nodes()
    assert [n.node_id for n in store.get_nodes(node_ids=[n.node_id for n in nodes])] == ["0b31ae71"]
    store.clear()
    assert store.get_nodes(node_ids=["0b31ae71"]) == []


def test_filters(store, nodes):
    store.add(nodes)

    def run(*filters, condition=FilterCondition.AND, doc_ids=None):
        q = VectorStoreQuery(
            query_embedding=[1.0, 0.0, 0.0],
            similarity_top_k=4,
            doc_ids=doc_ids,
            filters=MetadataFilters(filters=list(filters), condition=condition),
        )
        return sorted(ids(store.query(q)))

    king = MetadataFilter(key="author", value="Stephen King")
    assert run(king) == ["c330d77f", "c3d1e1dd"]
    assert run(MetadataFilter(key="author", value="Stephen King", operator=FilterOperator.NE)) == ["0b31ae71", "c3ew11cd"]
    assert run(MetadataFilter(key="year", value=1974, operator=FilterOperator.GT)) == ["0b31ae71", "c330d77f"]
    assert run(MetadataFilter(key="year", value=1974, operator=FilterOperator.GTE)) == ["0b31ae71", "c330d77f", "c3d1e1dd"]
    assert run(MetadataFilter(key="year", value=1974, operator=FilterOperator.LT)) == ["c3ew11cd"]
    assert run(MetadataFilter(key="year", value=1974, operator=FilterOperator.LTE)) == ["c3d1e1dd", "c3ew11cd"]
    assert run(MetadataFilter(key="theme", value=["Mafia"], operator=FilterOperator.IN)) == ["c3d1e1dd", "c3ew11cd"]
    assert run(MetadataFilter(key="theme", value=["Mafia"], operator=FilterOperator.NIN)) == ["0b31ae71", "c330d77f"]
    mafia = MetadataFilter(key="theme", value="Mafia")
    assert run(king, mafia) == ["c3d1e1dd"]
    assert run(king, mafia, condition=FilterCondition.OR) == ["c330d77f", "c3d1e1dd", "c3ew11cd"]
    nested = MetadataFilters(filters=[mafia, MetadataFilter(key="year", value=1950, operator=FilterOperator.LT)])
    assert run(king, nested, condition=FilterCondition.OR) == ["c330d77f", "c3d1e1dd", "c3ew11cd"]
    assert run(king, doc_ids=["test-1", "test-3"]) == ["c3d1e1dd"]
    with pytest.raises(ValueError):
        run(MetadataFilter(key="publisher", value="x"))


def test_an_index_on_top(store):
    embed = MockEmbedding(embed_dim=8)
    index = VectorStoreIndex.from_documents(
        [Document(text="fenecdb is a database", doc_id="d1"), Document(text="a desert fox", doc_id="d2")],
        storage_context=StorageContext.from_defaults(vector_store=store),
        embed_model=embed,
    )
    found = index.as_retriever(similarity_top_k=2).retrieve("database")
    assert sorted(n.node.ref_doc_id for n in found) == ["d1", "d2"]
    index.delete_ref_doc("d1")
    found = index.as_retriever(similarity_top_k=2).retrieve("database")
    assert [n.node.ref_doc_id for n in found] == ["d2"]
