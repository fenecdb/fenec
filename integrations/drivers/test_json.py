"""A `json` field is PostgreSQL's `jsonb` to psycopg and asyncpg: its cells
decode into the dicts and lists they hold, in text and in binary -- where
`jsonb_send` puts a version byte before the text -- a parameter goes in as
`Jsonb` or as JSON text, a path filters, and COPY carries the text."""

import asyncio
import io
import json
import os
import uuid

import asyncpg
import psycopg
from psycopg.conninfo import conninfo_to_dict
from psycopg.types.json import Jsonb

DSN = os.environ.get("FENEC_PG", "host=127.0.0.1 port=5433 user=fenec dbname=fenec")

META = {"lang": "tr", "source": {"site": "x", "rank": 3}, "tags": ["ai", "db"], "price": 19.99}


def test_psycopg_reads_and_writes_jsonb():
    t = f"js_{uuid.uuid4().hex[:12]}"
    with psycopg.connect(DSN, autocommit=True) as c:
        c.execute(f"create collection {t} (title text, meta json)")
        c.execute(f"put {t} {{title: %s, meta: %s}}", ("a", Jsonb(META)))
        c.execute(f"put {t} {{title: %s, meta: %s}}", ("b", Jsonb([1, 2.5, 12345678901])))
        c.execute(f"put {t} {{title: %s, meta: %s}}", ("c", json.dumps({"lang": "en"})))
        # Text, as psycopg asks by default: a dict, a list, and a path's value.
        rows = c.execute(f"get {t} select title, meta, meta.lang order id").fetchall()
        assert rows == [
            ("a", META, "tr"),
            ("b", [1, 2.5, 12345678901], None),
            ("c", {"lang": "en"}, "en"),
        ]
        # Binary, as a binary cursor asks: the version byte, then the text.
        with c.cursor(binary=True) as cur:
            cur.execute(f"get {t} select meta, meta.source where title = %s", ("a",))
            assert cur.fetchone() == (META, {"site": "x", "rank": 3})
        # A path filters and orders; a string compared with one is text.
        assert c.execute(
            f"get {t} select title where meta.lang = %s", ("tr",)
        ).fetchall() == [("a",)]
        assert c.execute(
            f"get {t} select title where meta.source.rank >= %s", (2,)
        ).fetchall() == [("a",)]
        c.execute(f"set {t} {{meta.lang: %s}} where title = %s", ("az", "a"))
        assert c.execute(f"get {t} select meta.lang where title = 'a'").fetchone() == ("az",)
        # COPY, in and out, carries the JSON text.
        with c.cursor().copy(f"COPY {t} (title, meta) FROM STDIN") as cp:
            cp.write_row(("d", json.dumps({"n": [1, {"x": None}]})))
        assert c.execute(f"get {t} select meta where title = 'd'").fetchone() == (
            {"n": [1, {"x": None}]},
        )
        out = io.BytesIO()
        with c.cursor().copy(f"COPY (get {t} select title, meta where title = 'd') TO STDOUT") as cp:
            for chunk in cp:
                out.write(chunk)
        assert out.getvalue() == b'd\t{"n":[1,{"x":null}]}\n'


def test_asyncpg_reads_and_writes_jsonb():
    async def go():
        d = conninfo_to_dict(DSN)
        c = await asyncpg.connect(
            host=d["host"],
            port=int(d["port"]),
            user=d["user"],
            password=d.get("password"),
            database=d["dbname"],
        )
        t = f"ja_{uuid.uuid4().hex[:12]}"
        try:
            await c.execute(f"create collection {t} (title text, meta json)")
            # Without a codec asyncpg hands jsonb over as its text, in and out
            # -- a binary COPY's cells too, the version byte before each.
            await c.execute(f"put {t} {{title: $1, meta: $2}}", "a", json.dumps(META))
            text = await c.fetchval(f"get {t} select meta where title = $1", "a")
            assert json.loads(text) == META
            await c.copy_records_to_table(
                t, records=[("c", json.dumps({"k": True}))], columns=["title", "meta"]
            )
            # With one, the values themselves; asyncpg asks every column in
            # binary, and sends a parameter in jsonb's binary form.
            await c.set_type_codec(
                "jsonb", encoder=json.dumps, decoder=json.loads, schema="pg_catalog"
            )
            await c.execute(f"put {t} {{title: $1, meta: $2}}", "b", {"lang": "en", "n": [1, 2]})
            rows = await c.fetch(f"get {t} select title, meta, meta.lang order id")
            assert [tuple(r) for r in rows] == [
                ("a", META, "tr"),
                ("c", {"k": True}, None),
                ("b", {"lang": "en", "n": [1, 2]}, "en"),
            ]
            # A path's parameter is jsonb, as `meta->'lang'`'s is.
            assert await c.fetchval(f"get {t} select title where meta.lang = $1", "en") == "b"
        finally:
            await c.close()

    asyncio.run(go())
