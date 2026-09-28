"""psycopg sends a string as text with no type named, and runs no
`Describe` of the statement before its Bind: fenec-pg reads the value as
the field its place names. Read by its look, "t" was a boolean and "42" a
number, which a text field refused."""

import os
import uuid

import psycopg

DSN = os.environ.get("FENEC_PG", "host=127.0.0.1 port=5433 user=fenec dbname=fenec")

NAMES = ["t", "42", "1.5", "[1,2]", "true"]


def test_a_string_is_the_text_it_is():
    t = f"tp_{uuid.uuid4().hex[:12]}"
    with psycopg.connect(DSN, autocommit=True) as c:
        c.execute(f"create collection {t} (name text, n int, ok bool)")
        for i, name in enumerate(NAMES):
            c.execute(f"put {t} {{name: %s, n: %s, ok: %s}}", (name, i, True))
        got = [r[0] for r in c.execute(f"get {t} select name order name").fetchall()]
        assert got == sorted(NAMES)
        # Compared with the text field, a string is text as well.
        assert c.execute(f"get {t} select n where name = %s", ("42",)).fetchone() == (1,)
        # And a number field's place reads a number, as before.
        assert c.execute(f"get {t} select name where n = %s", ("3",)).fetchone() == ("[1,2]",)
