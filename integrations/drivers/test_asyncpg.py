"""asyncpg over fenec-server's pg wire. asyncpg asks every column in binary
and sends every parameter as the type `Describe` names for it -- which
takes a type fenec-server reports rather than the unspecified OID 0, where
asyncpg ran a catalog introspection query of its own and failed."""

import asyncio
import io
import os
import uuid
from datetime import datetime, timezone

import asyncpg
from psycopg.conninfo import conninfo_to_dict

DSN = os.environ.get("FENEC_PG", "host=127.0.0.1 port=5433 user=fenec dbname=fenec")


def run(test):
    async def go():
        d = conninfo_to_dict(DSN)
        c = await asyncpg.connect(
            host=d["host"],
            port=int(d["port"]),
            user=d["user"],
            password=d.get("password"),
            database=d["dbname"],
        )
        name = f"ap_{uuid.uuid4().hex[:12]}"
        await c.execute(
            f"create collection {name} (name text, n int, score float, ok bool, at timestamp, raw bytes, e vector<2>)"
        )
        try:
            await test(c, name)
        finally:
            await c.close()

    asyncio.run(go())


def test_typed_parameters_and_rows():
    async def test(c, t):
        at = datetime(2026, 9, 28, 12, 30, tzinfo=timezone.utc)
        await c.execute(
            f"put {t} {{name: $1, n: $2, score: $3, ok: $4, at: $5, raw: $6, e: $7}}",
            "a", 7, 0.25, True, at, b"\x00\xff", "[1,0.5]",
        )
        row = await c.fetchrow(f"get {t} select name, n, score, ok, at, raw, e where n = $1", 7)
        assert tuple(row) == ("a", 7, 0.25, True, at, b"\x00\xff", "[1,0.5]")
        assert await c.fetchval(f"get {t} count") == 1
        assert await c.fetchval(f"get {t} select n where name = $1 and score > $2", "a", 0.1) == 7

    run(test)


def test_prepared_statements_executemany_and_transactions():
    async def test(c, t):
        await c.executemany(f"put {t} {{name: $1, n: $2}}", [(f"m{i}", i) for i in range(5)])
        ps = await c.prepare(f"get {t} select name where n = $1")
        assert [await ps.fetchval(i) for i in (1, 3)] == ["m1", "m3"]
        try:
            async with c.transaction():
                await c.execute(f"put {t} {{name: $1, n: $2}}", "gone", 99)
                raise RuntimeError("put back")
        except RuntimeError:
            pass
        assert await c.fetchval(f"get {t} count") == 5

    run(test)


def test_copy_to_table_in_text():
    async def test(c, t):
        res = await c.copy_to_table(
            t, source=io.BytesIO(b"r1\t100\nr2\t101\n"), columns=["name", "n"], format="text"
        )
        assert res == "COPY 2"
        assert await c.fetchval(f"get {t} count") == 2

    run(test)


def test_copy_records_to_table_in_binary():
    async def test(c, t):
        at = datetime(2026, 9, 28, 12, 30, tzinfo=timezone.utc)
        records = [(f"r{i}", i, i / 4, i % 2 == 0, at) for i in range(1000)]
        res = await c.copy_records_to_table(
            t, records=records, columns=["name", "n", "score", "ok", "at"]
        )
        assert res == "COPY 1000"
        row = await c.fetchrow(f"get {t} select name, n, score, ok, at where n = $1", 7)
        assert tuple(row) == ("r7", 7, 1.75, False, at)

    run(test)
