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


def test_a_write_that_does_not_write_its_count_is_refused_and_put_back(client):
    name = fresh("require")
    client.query(f"create collection {name} (name text, balance int)")
    try:
        client.collection(name).insert({"name": "a", "balance": 10})
        accounts = client.collection(name)
        with pytest.raises(FenecError) as e:
            accounts.where("name", "nobody").update({"balance": 0}, require=1)
        assert e.value.status == 412
        assert "requires 1" in str(e.value)
        assert accounts.where("name", "a").update({"balance": 5}, require=1) == 1
        # A batch whose second write is unmet keeps nothing of the first.
        met = accounts.where("name", "a").to_update({"balance": 0}, require=1)
        unmet = accounts.where("name", "nobody").to_delete(require=1)
        with pytest.raises(FenecError) as e:
            client.batch([met, unmet])
        assert e.value.status == 412
        # The second statement stopped it, and nothing of the batch stayed.
        assert (e.value.at, e.value.completed) == (1, 0)
        assert client.query(f"get {name} select balance") == [{"balance": 5}]
    finally:
        client.query(f"drop collection if exists {name}")


def test_a_range_facet_answers_its_bounds_and_a_disjunctive_one_counts_past_its_filter(client):
    name = fresh("ranges")
    client.query(f"create collection {name} (price float, kind text)")
    try:
        client.query(f"put {name} [{{price: 5, kind: \"a\"}}, {{price: 30, kind: \"b\"}}, {{price: 40, kind: \"b\"}}]")
        q = (client.collection(name).where("kind", "a")
             .facet("price", ranges=[0, 25, 50.5]).facet("kind", disjunctive=True).limit(0))
        facets = q.rows().facets
        assert facets["price"] == [([0, 25], 1), ([25, 50.5], 0)]
        assert facets["kind"] == [("b", 2), ("a", 1)]
    finally:
        client.query(f"drop collection if exists {name}")


def test_a_refusal_outside_a_batch_names_no_statement(client):
    with pytest.raises(FenecError) as e:
        client.query("get no_such_collection_here")
    assert (e.value.at, e.value.completed) == (None, None)


def test_an_idempotency_key_makes_a_write_once(client):
    name = fresh("idem")
    client.query(f"create collection {name} (t text)")
    try:
        put = f"put {name} {{t: $1}}"
        assert client.query(put, ["once"], idempotency_key=f"{name}-1") == {"affected": 1}
        assert not client.replayed
        seq = client.seq
        assert client.query(put, ["once"], idempotency_key=f"{name}-1") == {"affected": 1}
        assert client.replayed
        assert client.query(f"get {name} count") == [{"count": 1}]
        with pytest.raises(FenecError) as e:
            client.query(put, ["another"], idempotency_key=f"{name}-1")
        assert e.value.status == 422

        # A copy carries its key on every write, the builder's too, and
        # shares the client's seq.
        keyed = client.with_idempotency_key(f"{name}-2")
        stmts = [(put, ["b1"]), (put, ["b2"])]
        first = keyed.batch(stmts)
        assert first["ok"] == 2 and not keyed.replayed
        assert client.seq > seq
        again = keyed.batch(stmts)
        assert again["ok"] == 2 and keyed.replayed
        assert client.query(f"get {name} count") == [{"count": 3}]
        assert client.with_idempotency_key(f"{name}-3").collection(name).insert({"t": "c"}) == 1
        assert client.with_idempotency_key(f"{name}-3").collection(name).insert({"t": "c"}) == 1
        assert client.query(f"get {name} count") == [{"count": 4}]
        with pytest.raises(ValueError):
            client.with_idempotency_key("")
    finally:
        client.query(f"drop collection if exists {name}")


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


def _server(plan):
    """A server on loopback that takes one connection after another and
    does, for each request it reads, what `plan` says next: "answer" with
    keep-alive, "close" (answer, then close the connection) or "drop"
    (close it with no answer). Its port, the requests it read, and an event
    set each time it closed a connection."""
    import socket
    import threading

    srv = socket.socket()
    srv.bind(("127.0.0.1", 0))
    srv.listen()
    seen = []
    closed = threading.Event()
    plan = list(plan)

    def read_request(conn):
        data = b""
        while b"\r\n\r\n" not in data:
            chunk = conn.recv(65536)
            if not chunk:
                return None
            data += chunk
        head, _, rest = data.partition(b"\r\n\r\n")
        length = 0
        for line in head.split(b"\r\n"):
            if line.lower().startswith(b"content-length:"):
                length = int(line.split(b":")[1])
        while len(rest) < length:
            rest += conn.recv(65536)
        return head + rest

    def serve():
        while plan:
            conn, _ = srv.accept()
            while plan:
                req = read_request(conn)
                if req is None:
                    break
                seen.append(req)
                what = plan.pop(0)
                if what == "drop":
                    break
                body = b'{"affected":1}'
                conn.sendall(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
                    b"Content-Length: %d\r\nConnection: keep-alive\r\n\r\n%s" % (len(body), body)
                )
                if what == "close":
                    break
            conn.close()
            closed.set()
        srv.close()

    threading.Thread(target=serve, daemon=True).start()
    return srv.getsockname()[1], seen, closed


def test_a_connection_the_server_closed_is_opened_again_before_a_request():
    from fenecdb import Client

    port, seen, closed = _server(["close", "answer"])
    c = Client(f"http://127.0.0.1:{port}")
    assert c.query("put t {n: 1}") == {"affected": 1}
    assert closed.wait(10)
    assert c.query("put t {n: 2}") == {"affected": 1}
    assert len(seen) == 2


def test_a_request_sent_is_never_sent_again():
    from fenecdb import Client

    port, seen, _ = _server(["answer", "drop", "answer"])
    c = Client(f"http://127.0.0.1:{port}")
    assert c.query("put t {n: 1}") == {"affected": 1}
    with pytest.raises(Exception):
        c.query("put t {n: 2}")
    # The server read the second write once: it was not sent again over a
    # new connection after the first broke under it.
    assert len(seen) == 2
    assert c.query("put t {n: 3}") == {"affected": 1}
    assert len(seen) == 3
