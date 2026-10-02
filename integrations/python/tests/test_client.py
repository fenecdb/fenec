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


def test_a_write_names_its_change_and_a_read_can_wait_for_it(client):
    name = fresh("seq")
    client.query(f"create collection {name} (t text)")
    try:
        client.query(f'put {name} {{t: "x"}}')
        assert client.seq and client.seq > 0
        # On the primary the write is there already: answered at once.
        assert client.query(f"get {name} count", after=client.seq) == [{"count": 1}]
        with pytest.raises(FenecError) as e:
            client.query(f"get {name} count", after="soon")
        assert e.value.status == 400
    finally:
        client.query(f"drop collection if exists {name}")


def test_a_schema_is_compared_and_applied_only_when_asked(client):
    from fenecdb import SchemaError

    name = fresh("schema")
    v1 = f"create collection {name} (title text required, n int @hash)"
    client.query(v1)
    try:
        assert client.schema(v1)["refusals"] == []
        # A field the server lacks: the server's to add, refused.
        v2 = f"create collection {name} (title text required, n int @hash, at timestamp)"
        with pytest.raises(SchemaError) as e:
            client.schema(v2)
        assert [(r["kind"], r["field"]) for r in e.value.refusals] == [("field_missing", "at")]
        # Asked, it is added: the code owns the schema.
        out = client.schema(v2, migrate=True)
        assert out["statements"] == [f"alter collection {name} add field at timestamp"]
        # A rename is a migration, run once.
        v3 = f"create collection {name} (name text required, n int @hash, at timestamp)"
        with pytest.raises(SchemaError):
            client.schema(v3, migrate=True)
        moved = [f"alter collection {name} rename field title to name"]
        assert client.schema(v3, moved, migrate=True)["migrations"] == [1]
        assert client.schema(v3, moved, migrate=True)["migrations"] == []
    finally:
        client.query(f"drop collection if exists {name}")
        client.query("drop collection if exists _migrations")
