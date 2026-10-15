"""`reads="nearest"`: the builder's reads, and a statement given
`read=True`, marked for a read replica near the client (`Fenec-Read:
replica`) and sent after the furthest position home named to it
(`Fenec-After`, `Fenec-History`), which a 504 from home lowers to where
home stands; writes, and every request of a client reading home, sent as
before. Against a scripted server: what a router does with the mark is
fenec-shard's to test, and the JavaScript client's against real routers."""

import asyncio
import json
import socket
import threading

import pytest

from fenecdb import AsyncClient, Client


def _scripted(answers):
    """A server on loopback answering each request it reads with the next
    of `answers` -- `(status, body, headers)` -- on one connection kept
    alive. Its port, and each request's headers as a dict, lower-cased."""
    srv = socket.socket()
    srv.bind(("127.0.0.1", 0))
    srv.listen()
    seen = []
    answers = list(answers)

    def read_request(conn):
        data = b""
        while b"\r\n\r\n" not in data:
            chunk = conn.recv(65536)
            if not chunk:
                return None
            data += chunk
        head, _, rest = data.partition(b"\r\n\r\n")
        headers = {}
        for line in head.split(b"\r\n")[1:]:
            k, _, v = line.decode().partition(":")
            headers[k.strip().lower()] = v.strip()
        length = int(headers.get("content-length", 0))
        while len(rest) < length:
            rest += conn.recv(65536)
        return headers

    def serve():
        while answers:
            conn, _ = srv.accept()
            while answers:
                req = read_request(conn)
                if req is None:
                    break
                seen.append(req)
                status, body, extra = answers.pop(0)
                raw = json.dumps(body).encode()
                head = f"HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n"
                head += f"Content-Length: {len(raw)}\r\nConnection: keep-alive\r\n"
                for k, v in extra.items():
                    head += f"{k}: {v}\r\n"
                conn.sendall(head.encode() + b"\r\n" + raw)
            conn.close()
        srv.close()

    threading.Thread(target=serve, daemon=True).start()
    return srv.getsockname()[1], seen


H1, H2 = "1-00000000000000aa", "2-00000000000000bb"


def _script():
    return [
        (200, {"affected": 1}, {"Fenec-Seq": "9", "Fenec-History": H1}),
        (200, {"affected": 1}, {"Fenec-Seq": "12", "Fenec-History": H1}),
        (200, [{"n": 1}], {}),
        (200, [{"count": 2}], {}),
        (200, [{"count": 2}], {}),
        # A read near's answer moves nothing: it may be a replica's.
        (200, {"ok": 1, "results": []}, {"Fenec-Seq": "15", "Fenec-History": H1}),
        # The lost write: no copy holds it, home either, which says where
        # it stands: the client's position from here on.
        (504, {"error": "change 12 is not in this copy's history", "seq": 10, "history": H2}, {}),
        (200, [{"n": 1}], {}),
        (200, [{"n": 1}], {}),
    ]


def _check(seen):
    put, insert, rows, count, text, batch, near, home, after = seen
    for w in (put, insert):
        assert "fenec-read" not in w and "fenec-after" not in w
    # The furthest position, not the last answered.
    for r in (rows, count, batch, near):
        assert r["fenec-read"] == "replica"
        assert r["fenec-after"] == "12"
        assert r["fenec-history"] == H1
    # A text not marked goes home.
    assert "fenec-read" not in text
    assert "fenec-read" not in home and "fenec-after" not in home
    assert "fenec-history" not in home
    # Home's position taken, lower and of the history after the failover.
    assert after["fenec-after"] == "10"
    assert after["fenec-history"] == H2


def test_a_client_reading_near_marks_its_reads_and_sends_its_writes_after():
    port, seen = _scripted(_script())
    db = Client(f"http://127.0.0.1:{port}", reads="nearest")
    db.query("put t {n: 1}")
    db.with_idempotency_key("k").collection("t").insert({"n": 2})
    db.seq = 9  # a write answered late is not the furthest
    assert db.collection("t").rows() == [{"n": 1}]
    assert db.collection("t").count() == 2
    assert db.query("get t count") == [{"count": 2}]
    db.batch([("get t count", [])], read=True)
    assert db.collection("t").rows() == [{"n": 1}]
    assert db.collection("t").rows() == [{"n": 1}]
    _check(seen)


def test_an_async_client_reading_near_does_the_same():
    port, seen = _scripted(_script())

    async def run():
        async with AsyncClient(f"http://127.0.0.1:{port}", reads="nearest") as db:
            await db.query("put t {n: 1}")
            await db.with_idempotency_key("k").collection("t").insert({"n": 2})
            assert await db.collection("t").rows() == [{"n": 1}]
            assert await db.collection("t").count() == 2
            assert await db.query("get t count") == [{"count": 2}]
            await db.batch([("get t count", [])], read=True)
            assert await db.collection("t").rows() == [{"n": 1}]
            assert await db.collection("t").rows() == [{"n": 1}]

    asyncio.run(run())
    _check(seen)


def test_a_client_reads_home_unless_it_asks_for_nearest():
    port, seen = _scripted([(200, [], {}), (200, [], {})])
    db = Client(f"http://127.0.0.1:{port}")
    db.collection("t").rows()
    db.query("get t count", read=True)
    assert all("fenec-read" not in r for r in seen)
    with pytest.raises(ValueError):
        Client("http://127.0.0.1:1", reads="anywhere")


def test_a_504_sets_the_position_back_over_its_own_alone_never_to_an_earlier_term():
    from fenecdb import _Shared

    s = _Shared()
    s.answered(100, H2)
    sent = s.at
    # A write lands at 101 while a read sent after 100 waits; the read's
    # 504 at 100 comes after it, and is not taken.
    s.answered(101, H2)
    s.told(json.dumps({"seq": 100, "history": H2}).encode(), sent)
    assert s.at[1:] == (101, H2)
    # An earlier term's -- the old primary, fenced -- is not taken either.
    sent = s.at
    s.told(json.dumps({"seq": 140, "history": H1}).encode(), sent)
    assert s.at[1:] == (101, H2)
    # A lower position of the same or a later term, over the one sent, is.
    s.told(json.dumps({"seq": 90, "history": H2}).encode(), sent)
    assert s.at[1:] == (90, H2)


def test_a_tenant_made_again_has_its_position_taken_its_term_lower():
    from fenecdb import _Shared

    # A tenant failed over twice: term 3 of its lineage, aaaaaaaa. Deleted
    # and made again, home names term 1 of another lineage: refused as an
    # earlier term, every read near waited its whole wait for good.
    old, remade, third = "3-aaaaaaaa000000c3", "1-bbbbbbbb00000001", "1-cccccccc00000001"
    s = _Shared()
    s.answered(50, old)
    s.told(json.dumps({"seq": 2, "history": remade}).encode(), s.at)
    assert s.at[1:] == (2, remade)
    # From a write's answer the same: home's word on the tenant now.
    s.answered(1, third)
    assert s.at[1:] == (1, third)
    # Within one lineage an earlier term is still refused.
    s.answered(9, "2-cccccccc00000002")
    s.answered(90, third)
    assert s.at[1:] == (9, "2-cccccccc00000002")


def test_a_change_the_caller_names_is_not_taken_from_its_504():
    port, seen = _scripted(
        [
            (200, {"affected": 1}, {"Fenec-Seq": "9", "Fenec-History": H1}),
            (504, {"error": "not in the wait", "seq": 3, "history": H2}, {}),
            (200, [], {}),
            (200, [], {}),
        ]
    )
    db = Client(f"http://127.0.0.1:{port}", reads="nearest")
    db.query("put t {n: 1}")
    db.query("get t count", read=True, after=50)
    db.collection("t").rows()
    assert seen[1]["fenec-after"] == "50"
    assert seen[3]["fenec-after"] == "9" and seen[3]["fenec-history"] == H1
