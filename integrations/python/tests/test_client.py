"""The client alone: a statement, a batch, and a refusal with its status."""

import pytest

from conftest import fresh
from fenecdb import FenecError


def test_a_statement_and_a_batch(client):
    name = fresh("client")
    client.query(f"create collection {name} (title text, n int)")
    try:
        assert client.query(f"put {name} {{title: $1, n: $2}}", ["a", 1]) == {"affected": 1}
        out = client.batch(
            [
                (f"put {name} {{title: $1, n: $2}}", ["b", 2]),
                (f"get {name} select title where n >= $1 order n", [1]),
            ]
        )
        assert out["ok"] == 2
        assert client.query(f"get {name} select title order n") == [
            {"title": "a"},
            {"title": "b"},
        ]
    finally:
        client.query(f"drop collection if exists {name}")


def test_a_refusal_says_why_and_how(client):
    with pytest.raises(FenecError) as e:
        client.query("get no_such_collection_here")
    assert e.value.status == 404
    assert "no_such_collection_here" in str(e.value)
    with pytest.raises(FenecError) as e:
        client.query("get x wher")
    assert e.value.status == 400
