"""The LangChain vector store against LangChain's own standard suite, and
what the suite does not ask: filters, relevance, `from_texts`."""

import pytest
from langchain_core.documents import Document
from langchain_tests.integration_tests import VectorStoreIntegrationTests

from conftest import TOKEN, URL, fresh
from fenecdb.langchain import FenecVectorStore


class TestFenecVectorStore(VectorStoreIntegrationTests):
    @pytest.fixture()
    def vectorstore(self):
        store = FenecVectorStore(self.get_embeddings(), fresh("lc"), url=URL, token=TOKEN)
        try:
            yield store
        finally:
            store.drop_collection()


@pytest.fixture()
def store():
    s = FenecVectorStore(
        VectorStoreIntegrationTests.get_embeddings(),
        fresh("lc"),
        url=URL,
        token=TOKEN,
        metadata_fields={"source": "text", "year": "int"},
    )
    try:
        yield s
    finally:
        s.drop_collection()


def test_a_filter_is_answered_by_the_index(store):
    store.add_documents(
        [
            Document(page_content="alpha", metadata={"source": "wiki", "year": 2024}),
            Document(page_content="beta", metadata={"source": "blog", "year": 2025}),
            Document(page_content="gamma", metadata={"source": "wiki", "year": 2025}),
        ],
        ids=["a", "b", "g"],
    )
    found = store.similarity_search("alpha", k=3, filter={"source": "wiki"})
    assert [d.id for d in found] == ["a", "g"]
    found = store.similarity_search("alpha", k=3, filter={"source": "wiki", "year": 2025})
    assert [d.id for d in found] == ["g"]
    found = store.similarity_search("beta", k=3, filter={"year": {"$in": [2025]}})
    assert [d.id for d in found] == ["b", "g"]
    with pytest.raises(ValueError):
        store.similarity_search("alpha", filter={"author": "x"})


def test_relevance_is_one_for_the_same_text(store):
    store.add_texts(["alpha", "beta"], ids=["a", "b"])
    ((best, score),) = store.similarity_search_with_relevance_scores("alpha", k=1)
    assert best.id == "a"
    assert score == pytest.approx(1.0, abs=1e-5)


def test_from_texts_and_delete_everything():
    embeddings = VectorStoreIntegrationTests.get_embeddings()
    s = FenecVectorStore.from_texts(
        ["one", "two"],
        embeddings,
        metadatas=[{"n": 1}, {"n": 2}],
        ids=["1", "2"],
        collection=fresh("lc"),
        url=URL,
        token=TOKEN,
    )
    try:
        assert sorted(d.metadata["n"] for d in s.get_by_ids(["1", "2"])) == [1, 2]
        s.delete()
        assert s.similarity_search("one", k=2) == []
    finally:
        s.drop_collection()


def test_names_that_would_reach_the_query_text_are_refused():
    embeddings = VectorStoreIntegrationTests.get_embeddings()
    for bad in ["docs; drop collection x", "1docs", ""]:
        with pytest.raises(ValueError):
            FenecVectorStore(embeddings, bad)
    with pytest.raises(ValueError):
        FenecVectorStore(embeddings, "ok", metadata_fields={"a b": "text"})
    with pytest.raises(ValueError):
        FenecVectorStore(embeddings, "ok", metadata_fields={"lc_id": "text"})
