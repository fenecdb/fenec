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


@pytest.fixture()
def worded():
    s = FenecVectorStore(
        VectorStoreIntegrationTests.get_embeddings(),
        fresh("lc"),
        url=URL,
        token=TOKEN,
        metadata_fields={"source": "text"},
        full_text=True,
        quant="int8",
    )
    try:
        yield s
    finally:
        s.drop_collection()


def test_text_and_hybrid_search(worded):
    worded.add_texts(
        [
            "compact rewrites the file without its dead records",
            "a checkpoint lands the graph in the file",
            "a replica follows its primary",
        ],
        metadatas=[{"source": "a"}, {"source": "b"}, {"source": "a"}],
        ids=["c", "k", "r"],
    )
    found = worded.similarity_search("dead records", k=3, mode="text")
    assert [d.id for d in found] == ["c"]
    # The words find "k" alone, so it leads whatever the vector ranks.
    found = worded.similarity_search("checkpoint graph", k=3, mode="hybrid")
    assert found[0].id == "k"
    found = worded.similarity_search("checkpoint graph", k=3, mode="hybrid", filter={"source": "a"})
    assert "k" not in [d.id for d in found]
    # Over int8 codes, a text's own vector is still its nearest.
    best = worded.similarity_search("a replica follows its primary", k=1)
    assert best[0].id == "r"
    retriever = worded.as_retriever(search_kwargs={"k": 1, "mode": "text"})
    assert [d.id for d in retriever.invoke("primary")] == ["r"]


def test_modes_and_codes_are_checked(store):
    store.add_texts(["alpha"], ids=["a"])
    with pytest.raises(ValueError):
        store.similarity_search("alpha", mode="text")
    with pytest.raises(ValueError):
        store.similarity_search("alpha", mode="bm25")
    embeddings = VectorStoreIntegrationTests.get_embeddings()
    with pytest.raises(ValueError):
        FenecVectorStore(embeddings, "ok", metric="l2", quant="bit")
    with pytest.raises(ValueError):
        FenecVectorStore(embeddings, "ok", quant="pq")
