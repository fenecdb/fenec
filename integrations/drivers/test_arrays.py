"""A list field is PostgreSQL's array over the wire: psycopg and asyncpg read
it as a Python list and bind one to it, where they had a string."""

import asyncio
import datetime
import os
import uuid

import asyncpg
import psycopg
from psycopg.conninfo import conninfo_to_dict

DSN = os.environ.get("FENEC_PG", "host=127.0.0.1 port=5433 user=fenec dbname=fenec")
TAGS = ["plain", 'a "q"', "b\\s", "x,y", "", "NULL"]


def schema(t):
    return f"create collection {t} (name text, tags [text], ns [int], fs [float], oks [bool], ats [timestamp])"


def test_psycopg_reads_and_binds_lists():
    t = f"ar_{uuid.uuid4().hex[:12]}"
    at = datetime.datetime(2026, 9, 28, 12, 30, tzinfo=datetime.timezone.utc)
    with psycopg.connect(DSN, autocommit=True) as c:
        c.execute(schema(t))
        c.execute(
            f"put {t} {{name: %s, tags: %s, ns: %s, fs: %s, oks: %s, ats: %s}}",
            ("a", TAGS, [1, -2, 3], [0.5, 1.25], [True, False], [at]),
        )
        for binary in (False, True):
            with c.cursor(binary=binary) as cur:
                row = cur.execute(
                    f"get {t} select tags, ns, fs, oks, ats where name = %s", ("a",)
                ).fetchone()
                assert row == (TAGS, [1, -2, 3], [0.5, 1.25], [True, False], [at]), binary
        # An element of it, compared.
        assert c.execute(f"get {t} select name where ns has %s", (-2,)).fetchone() == ("a",)


def test_asyncpg_reads_and_binds_lists():
    async def run():
        t = f"aa_{uuid.uuid4().hex[:12]}"
        d = conninfo_to_dict(DSN)
        c = await asyncpg.connect(
            host=d["host"],
            port=int(d["port"]),
            user=d["user"],
            password=d.get("password"),
            database=d["dbname"],
        )
        try:
            await c.execute(schema(t))
            await c.execute(f"put {t} {{name: $1, tags: $2, ns: $3}}", "a", TAGS, [7, None, 9])
            row = await c.fetchrow(f"get {t} select tags, ns where name = $1", "a")
            assert list(row["tags"]) == TAGS
            assert list(row["ns"]) == [7, None, 9]
        finally:
            await c.close()

    asyncio.run(run())

