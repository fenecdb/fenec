"""pgvector-python over fenec-server's pg wire, with psycopg and with asyncpg:
`register_vector` finds `vector`, `halfvec` and `sparsevec` by name in the
catalog, and NumPy arrays, `HalfVector` and `SparseVector` go both ways in
pgvector's binary format -- as they do against PostgreSQL with pgvector."""

import asyncio
import os
import uuid

import asyncpg
import numpy as np
import psycopg
import pytest
from pgvector import HalfVector, SparseVector
from pgvector.asyncpg import register_vector as register_vector_async
from pgvector.psycopg import register_vector
from psycopg.conninfo import conninfo_to_dict

DSN = os.environ.get("FENEC_PG", "host=127.0.0.1 port=5433 user=fenec dbname=fenec")


@pytest.fixture()
def table() -> str:
    """A collection no other test uses, with a field of each type."""
    name = f"pv_{uuid.uuid4().hex[:12]}"
    with psycopg.connect(DSN, autocommit=True) as c:
        c.execute(
            f"create collection {name} (name text, e vector<3> @hnsw(cosine), "
            f"h vector<3, f16>, s sparse<5> @inverted)"
        )
    return name


def sparse(v: SparseVector):
    return v.dimensions(), v.indices(), v.values()


def test_psycopg_register_vector(table):
    with psycopg.connect(DSN, autocommit=True) as c:
        register_vector(c)
        e = np.array([1.0, 2.0, 3.0], dtype=np.float32)
        h = HalfVector([1.5, 2.0, 3.0])
        s = SparseVector({0: 1.0, 3: 0.5}, 5)
        c.execute(f"put {table} {{name: %s, e: %s, h: %s, s: %s}}", ("a", e, h, s))
        for cur in (c.cursor(), c.cursor(binary=True)):
            name, got_e, got_h, got_s = cur.execute(
                f"get {table} select name, e, h, s where name = %s", ("a",)
            ).fetchone()
            assert name == "a"
            assert got_e.to_list() == [1.0, 2.0, 3.0]
            assert got_h.to_list() == [1.5, 2.0, 3.0]
            assert sparse(got_s) == sparse(s)
        hit = c.execute(f"get {table} select name near e %s limit 1", (e,)).fetchone()
        assert hit[0] == "a"
        hit = c.execute(
            f"get {table} select name near s %s limit 1", (SparseVector({0: 1.0}, 5),)
        ).fetchone()
        assert hit[0] == "a"
        c.cursor().executemany(
            f"put {table} {{name: %s, e: %s}}",
            [(f"m{i}", np.array([i, 1.0, 2.0], dtype=np.float32)) for i in range(5)],
        )
        assert c.execute(f"get {table} count").fetchone()[0] == 6


def test_psycopg_binary_copy_with_set_types(table):
    """pgvector-python's bulk load: a binary COPY, the types named."""
    with psycopg.connect(DSN, autocommit=True) as c:
        register_vector(c)
        with c.cursor().copy(f"COPY {table} (name, e, h, s) FROM STDIN WITH (FORMAT BINARY)") as copy:
            copy.set_types(["text", "vector", "halfvec", "sparsevec"])
            for i in range(100):
                copy.write_row(
                    [
                        f"c{i}",
                        np.array([1.0, i, 2.0], dtype=np.float32),
                        HalfVector([i % 8, 1.0, 1.0]),
                        SparseVector({i % 5: 1.0}, 5),
                    ]
                )
        e, h, s = c.execute(f"get {table} select e, h, s where name = %s", ("c7",)).fetchone()
        assert e.to_list() == [1.0, 7.0, 2.0]
        assert h.to_list() == [7.0, 1.0, 1.0]
        assert sparse(s) == sparse(SparseVector({2: 1.0}, 5))


def test_without_register_vector_a_vector_is_its_text(table):
    with psycopg.connect(DSN, autocommit=True) as c:
        c.execute(f"put {table} {{name: %s, e: %s, s: %s}}", ("p", "[1,2,3]", "{1:1}/5"))
        row = c.execute(f"get {table} select e, s where name = %s", ("p",)).fetchone()
        assert row == ("[1,2,3]", "{1:1}/5")


def test_asyncpg_register_vector(table):
    async def go():
        d = conninfo_to_dict(DSN)
        c = await asyncpg.connect(
            host=d["host"],
            port=int(d["port"]),
            user=d["user"],
            password=d.get("password"),
            database=d["dbname"],
        )
        try:
            await register_vector_async(c)
            e = np.array([3.0, 2.0, 1.0], dtype=np.float32)
            h = HalfVector([1.0, 2.0, 3.0])
            s = SparseVector({1: 2.0}, 5)
            await c.execute(f"put {table} {{name: $1, e: $2, h: $3, s: $4}}", "b", e, h, s)
            row = await c.fetchrow(f"get {table} select e, h, s where name = $1", "b")
            assert row["e"].to_list() == [3.0, 2.0, 1.0]
            assert row["h"].to_list() == [1.0, 2.0, 3.0]
            assert sparse(row["s"]) == sparse(s)
            hit = await c.fetchrow(f"get {table} select name near e $1 limit 1", e)
            assert hit["name"] == "b"
            hit = await c.fetchrow(f"get {table} select name near s $1 limit 1", SparseVector({1: 1.0}, 5))
            assert hit["name"] == "b"
            await c.executemany(
                f"put {table} {{name: $1, e: $2}}",
                [(f"x{i}", np.array([1.0, i, 3.0], dtype=np.float32)) for i in range(5)],
            )
            await c.copy_records_to_table(
                table,
                records=[(f"r{i}", np.array([i, 2.0, 1.0], dtype=np.float32)) for i in range(5)],
                columns=["name", "e"],
            )
            got = await c.fetchval(f"get {table} select e where name = $1", "r2")
            assert got.to_list() == [2.0, 2.0, 1.0]
            assert await c.fetchval(f"get {table} count") == 11
        finally:
            await c.close()

    asyncio.run(go())
