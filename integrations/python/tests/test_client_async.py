"""AsyncClient against the same server: a statement, a batch, a refusal,
calls made at once over one client, and a connection the server closed."""

import asyncio

import pytest

from conftest import TOKEN, URL, fresh
from fenecdb import AsyncClient, FenecError


def run(coro):
    return asyncio.run(coro)


def test_a_statement_and_a_batch():
    async def go():
        async with AsyncClient(URL, TOKEN) as db:
            name = fresh("aclient")
            await db.query(f"create collection {name} (title text, n int)")
            try:
                assert await db.query(f"put {name} {{title: $1, n: $2}}", ["a", 1]) == {"affected": 1}
                out = await db.batch(
                    [
                        (f"put {name} {{title: $1, n: $2}}", ["b", 2]),
                        (f"get {name} select title where n >= $1 order n", [1]),
                    ]
                )
                assert out["ok"] == 2
                assert await db.query(f"get {name} select title order n") == [
                    {"title": "a"},
                    {"title": "b"},
                ]
            finally:
                await db.query(f"drop collection if exists {name}")

    run(go())


def test_a_refusal_says_why():
    async def go():
        async with AsyncClient(URL, TOKEN) as db:
            with pytest.raises(FenecError) as e:
                await db.query("get no_such_collection_here")
            assert e.value.status == 404
            with pytest.raises(FenecError) as e:
                await db.query("get x wher")
            assert e.value.status == 400
            # The connection goes on after a refusal.
            assert await db.query("collections") is not None

    run(go())


def test_an_idempotency_key_and_where_a_batch_stopped():
    async def go():
        async with AsyncClient(URL, TOKEN) as db:
            name = fresh("aidem")
            await db.query(f"create collection {name} (t text)")
            try:
                put = f"put {name} {{t: $1}}"
                keyed = db.with_idempotency_key(f"{name}-1")
                assert (await keyed.batch([(put, ["a"]), (put, ["b"])]))["ok"] == 2
                assert not db.replayed and db.seq
                assert (await keyed.batch([(put, ["a"]), (put, ["b"])]))["ok"] == 2
                assert db.replayed
                await db.query(put, ["c"], idempotency_key=f"{name}-2")
                await db.query(put, ["c"], idempotency_key=f"{name}-2")
                assert await db.query(f"get {name} count") == [{"count": 3}]
                unmet = f"del {name} where t = $1 require 1"
                with pytest.raises(FenecError) as e:
                    await db.batch([(put, ["d"]), (unmet, ["none"])])
                assert (e.value.status, e.value.at, e.value.completed) == (412, 1, 0)
                assert await db.query(f"get {name} count") == [{"count": 3}]
            finally:
                await db.query(f"drop collection if exists {name}")

    run(go())


def test_calls_at_once_over_one_client_and_a_closed_connection():
    async def go():
        async with AsyncClient(URL, TOKEN) as db:
            name = fresh("aconc")
            await db.query(f"create collection {name} (n int)")
            try:
                await asyncio.gather(*(db.query(f"put {name} {{n: $1}}", [i]) for i in range(50)))
                assert await db.query(f"get {name} count") == [{"count": 50}]
                # The server's side of the kept connection gone: the next call
                # opens another.
                _, writer = db._s.conn
                writer.close()
                assert await db.query(f"get {name} count") == [{"count": 50}]
            finally:
                await db.query(f"drop collection if exists {name}")

    run(go())


def test_not_an_http_url():
    with pytest.raises(ValueError):
        AsyncClient("postgres://127.0.0.1:5433")
