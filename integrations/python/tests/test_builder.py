"""The query builder: held to integrations/builder-golden.json -- the text and
parameters the JavaScript builder makes of every chain there -- and run
against the server, where each answer is the answer to the same statement
written by hand."""

import array
import asyncio
import json
import os
from datetime import datetime
from pathlib import Path

import pytest

from conftest import TOKEN, URL, fresh
from fenecdb import AsyncClient, FacetCount, FenecError, Query, and_, collection, not_, or_, raw

# run-tests.sh mounts the file beside the package in its container.
GOLDEN = Path(os.environ.get("FENEC_GOLDEN") or Path(__file__).parents[2] / "builder-golden.json")
CASES = json.loads(GOLDEN.read_text(encoding="utf-8"))


def arg(x):
    """A step's argument as this builder takes it."""
    if isinstance(x, list):
        return [arg(v) for v in x]
    if not isinstance(x, dict):
        return x
    if "$or" in x:
        return or_(*[arg(c) for c in x["$or"]])
    if "$and" in x:
        return and_(*[arg(c) for c in x["$and"]])
    if "$not" in x:
        return not_(arg(x["$not"]))
    if "$raw" in x:
        return raw(x["$raw"][0], *[arg(p) for p in x["$raw"][1:]])
    if "$date" in x:
        return datetime.fromisoformat(x["$date"].replace("Z", "+00:00"))
    if "$f32" in x:
        return array.array("f", x["$f32"])
    return {k: arg(v) for k, v in x.items()}


SNAKE = {"parentKey": "parent_key"}


def kwargs(opts):
    return {SNAKE.get(k, k): arg(v) for k, v in (opts or {}).items()}


class Recorder:
    """A client that answers as the server would and keeps what it was sent."""

    def __init__(self):
        self.sent = []

    def query(self, text, params=None):
        self.sent.append((text, params))
        if text.split(" ", 1)[0] in ("put", "set", "del"):
            return {"affected": 0}
        return [{"count": 0}] if text.endswith(" count") else []


ENDPOINTS = {"rows", "first", "count", "explain", "insert", "update", "delete"}


def run(steps):
    rec = Recorder()
    first, *rest = steps
    assert first["op"] == "from"
    try:
        q = Query(*[arg(a) for a in first.get("args", [])], rec)
        for i, step in enumerate(rest):
            op, args = step["op"], step.get("args", [])
            last = i == len(rest) - 1
            if op == "toFenecQL":
                text, params = q.to_fenecql()
            elif op in ("toInsert", "insert"):
                fn = q.to_insert if op == "toInsert" else q.insert
                out = fn(arg(args[0]))
                text, params = out if op == "toInsert" else rec.sent[-1]
            elif op in ("toUpdate", "update"):
                fn = q.to_update if op == "toUpdate" else q.update
                out = fn(arg(args[0]), **kwargs(args[1] if len(args) > 1 else None))
                text, params = out if op == "toUpdate" else rec.sent[-1]
            elif op in ("toDelete", "delete"):
                fn = q.to_delete if op == "toDelete" else q.delete
                out = fn(**kwargs(args[0] if args else None))
                text, params = out if op == "toDelete" else rec.sent[-1]
            elif op in ENDPOINTS:
                getattr(q, op)()
                text, params = rec.sent[-1]
            else:
                q = step_of(q, op, args)
                continue
            assert last, f"{op} ends a chain"
            assert len(rec.sent) <= 1
            return {"text": text, "params": json.loads(json.dumps(params))}
        raise AssertionError("a chain ends with a statement")
    except FenecError as e:
        assert e.status == 0, e
        return {"error": str(e)}


def step_of(q, op, args):
    if op == "select":
        return q.select(*args)
    if op == "where":
        return q.where(*[arg(a) for a in args])
    if op == "orWhere":
        return q.or_where(*[arg(a) for a in args])
    if op in ("near", "rerank"):
        return getattr(q, op)(args[0], arg(args[1]), **kwargs(args[2] if len(args) > 2 else None))
    if op == "fuse":
        return q.fuse(**kwargs(args[0] if args else None))
    if op == "lookup":
        return q.lookup(args[0], **kwargs(args[1] if len(args) > 1 else None))
    if op == "order":
        return q.order(*args[:2], **kwargs(args[2] if len(args) > 2 else None))
    if op == "highlight":
        return q.highlight(args[0], **kwargs(args[1] if len(args) > 1 else None))
    if op == "snippet":
        return q.snippet(args[0], args[1], **kwargs(args[2] if len(args) > 2 else None))
    if op == "facet":
        return q.facet(args[0], **kwargs(args[1] if len(args) > 1 else None))
    if op in ("match", "group", "limit", "offset"):
        return getattr(q, op)(*args)
    raise AssertionError(f"no builder step {op}")


def test_the_golden_file_holds_enough_cases():
    assert len(CASES) >= 60
    assert sum("error" in k for k in CASES) >= 20


@pytest.mark.parametrize("case", CASES, ids=[k["name"] for k in CASES])
def test_a_golden_case(case):
    want = {"error": case["error"]} if "error" in case else {"text": case["text"], "params": case["params"]}
    assert run(case["steps"]) == want


def test_a_query_is_immutable_and_can_be_branched():
    base = collection("articles").where("year", ">=", 2024)
    a = base.where("tags", "has", "rust")
    b = base.limit(3)
    assert base.to_fenecql() == ("get articles where year >= $1", [2024])
    assert a.to_fenecql()[0] == "get articles where year >= $1 and tags has $2"
    assert b.to_fenecql()[0] == "get articles where year >= $1 limit 3"


def test_an_unbound_query_has_text_but_cannot_run():
    q = collection("t")
    assert q.to_fenecql() == ("get t", [])
    with pytest.raises(FenecError, match="not bound"):
        q.rows()


def test_a_tuple_a_numpy_like_list_and_a_naive_datetime_are_values():
    class Vec:
        def tolist(self):
            return [0.5, 0.25]

    q = collection("t").where("v", "=", Vec()).where("t", "in", ("a", "b"))
    q = q.where("at", ">=", datetime(2026, 1, 2, 3, 4, 5, 678000))
    assert q.to_fenecql()[1] == [[0.5, 0.25], "a", "b", "2026-01-02T03:04:05.678Z"]
    with pytest.raises(FenecError, match="cannot be used as a fenecdb value"):
        collection("t").where("v", "=", object()).to_fenecql()


# ---------------------------------------------------------- against the server


@pytest.fixture()
def shelf(client):
    name = fresh("shelf")
    notes = fresh("notes")
    client.query(
        f"create collection {name} (title text, year int @sorted, lang text @hash, "
        "tags [text], body text @text, embed vector<3> @hnsw(cosine))"
    )
    client.query(f"create collection {notes} (doc_id int @hash, stars int)")
    docs = client.collection(name)
    assert (
        docs.insert(
            [
                {"title": "Night at the oasis", "year": 2024, "lang": "en", "tags": ["desert"],
                 "body": "a night under the stars at the oasis", "embed": [0.1, 0.2, 0.3]},
                {"title": "Dunes", "year": 2021, "lang": "en", "tags": ["desert", "sand"],
                 "body": "dunes move with the wind", "embed": [0.9, 0.1, 0.0]},
                {"title": "Kum", "year": 2023, "lang": "tr", "tags": ["sand"],
                 "body": "kum ve rüzgar", "embed": [0.2, 0.8, 0.1]},
            ]
        )
        == 3
    )
    client.collection(notes).insert([{"doc_id": 1, "stars": 5}, {"doc_id": 1, "stars": 3}, {"doc_id": 3, "stars": 4}])
    try:
        yield name, notes
    finally:
        client.query(f"drop collection if exists {name}")
        client.query(f"drop collection if exists {notes}")


def test_builder_answers_are_the_texts_answers(client, shelf):
    name, notes = shelf
    docs = client.collection(name)
    v = [0.1, 0.2, 0.3]
    pairs = [
        (docs.select("title").where("year", ">=", 2022).order("year", "desc"),
         f"get {name} select title where year >= $1 order year desc", [2022]),
        (docs.select("title").where({"lang": "en", "tags": {"has": "sand"}}),
         f"get {name} select title where lang = $1 and tags has $2", ["en", "sand"]),
        (docs.select("title").where(or_({"lang": "tr"}, {"year": {"lt": 2022}})).order("title"),
         f"get {name} select title where lang = $1 or year < $2 order title asc", ["tr", 2022]),
        (docs.select("title").near("embed", v).limit(2),
         f"get {name} select title near embed $1 limit 2", [v]),
        (docs.select("title").match("body", "oasis stars"),
         f"get {name} select title match body $1", ["oasis stars"]),
        (docs.select("title").where("id", "in", [1, 3]).lookup(notes, on="doc_id", select=["stars"], order=[("stars", "desc")]),
         f"get {name} select title where id in [$1, $2] lookup {notes} on doc_id select stars order stars desc", [1, 3]),
        (docs.select("lang", "count(*)").group("lang").order("lang"),
         f"get {name} select lang, count(*) group lang order lang asc", []),
    ]
    for q, text, params in pairs:
        assert q.to_fenecql() == (text, params)
        assert q.rows() == client.query(text, params)
        assert q.rows()  # an answer worth comparing
    assert docs.where("lang", "en").count() == client.query(f"get {name} where lang = $1 count", ["en"])[0]["count"] == 2
    assert docs.order("year").first()["title"] == "Dunes"
    assert docs.where("lang", "xx").first() is None
    assert any("near" in line or "hnsw" in line.lower() for line in docs.near("embed", v).limit(1).explain())


def test_builder_writes_are_the_texts_writes(client, shelf):
    name, _ = shelf
    docs = client.collection(name)
    assert docs.where("lang", "tr").update({"year": 2025}) == 1
    assert client.query(f"get {name} select year where lang = $1", ["tr"]) == [{"year": 2025}]
    with pytest.raises(FenecError) as e:
        docs.delete()
    assert e.value.status == 0
    assert docs.where("year", "<", 2022).delete() == 1
    assert docs.count() == 2
    assert docs.delete(all=True) == 2
    assert docs.count() == 0


def test_the_async_builder_awaits_the_same_answers(client, shelf):
    name, notes = shelf

    async def go():
        async with AsyncClient(URL, TOKEN) as db:
            docs = db.collection(name)
            q = docs.select("title").where("year", ">=", 2022).order("year", "desc")
            assert await q.rows() == client.query(*q.to_fenecql())
            assert await docs.where("lang", "en").count() == 2
            assert (await docs.order("year").first())["title"] == "Dunes"
            assert await db.collection(notes).where("doc_id", 3).update({"stars": 1}) == 1
            assert await db.collection(notes).insert({"doc_id": 2, "stars": 2}) == 1
            assert await db.collection(notes).where("stars", "<=", 2).delete() == 2

    asyncio.run(go())


def test_marks_and_facets_come_back_where_the_server_puts_them(client):
    name = fresh("marks")
    client.query(f"create collection {name} (body text @text, kind text, meta json)")
    try:
        client.collection(name).insert(
            [
                {"body": "rust and go", "kind": "lang", "meta": {"lang": "en"}},
                {"body": "rust compiler", "kind": "tool", "meta": {"lang": "en"}},
                {"body": "python", "kind": "lang", "meta": {"lang": "tr"}},
            ]
        )
        docs = client.collection(name)
        rows = docs.select("kind").highlight("body").match("body", "rust").rows()
        # Offsets into the text, UTF-16 code units: `rust` is the first four.
        assert sorted((r["kind"], r["highlight(body)"]) for r in rows) == [
            ("lang", [[0, 4]]),
            ("tool", [[0, 4]]),
        ]
        assert rows.facets == {}
        tagged = docs.select("kind").highlight("body", pre="<b>", post="</b>").match("body", "rust")
        assert sorted(r["highlight(body)"] for r in tagged.rows()) == [
            "<b>rust</b> and go",
            "<b>rust</b> compiler",
        ]
        snip = docs.snippet("body", 5).match("body", "compiler").first()["snippet(body)"]
        assert snip == {"marks": [[5, 13]], "text": "rust compiler"}

        # Counted over every row matched, the page holding one of them.
        page = docs.select("kind").facet("kind").facet("meta.lang", top=1).order("kind").limit(1).rows()
        assert page == [{"kind": "lang"}]
        assert page.facets == {
            "kind": [FacetCount("lang", 2), FacetCount("tool", 1)],
            "meta.lang": [FacetCount("en", 2)],
        }
        assert docs.facet("kind").count() == 3
        # The client hands the same: `Rows` for a query with facets, the
        # bare list for one without, and a batch's result as it came.
        got = client.query(f"get {name} select kind order kind limit 1 facet kind")
        assert got == [{"kind": "lang"}] and got.facets["kind"][0] == ("lang", 2)
        assert type(client.query(f"get {name} select kind")) is list
        out = client.batch([(f"get {name} select kind limit 1 facet kind", [])])
        assert out["results"][0]["facets"]["kind"] == [
            {"value": "lang", "count": 2},
            {"value": "tool", "count": 1},
        ]

        async def go():
            async with AsyncClient(URL, TOKEN) as db:
                got = await db.collection(name).where("kind", "lang").facet("meta.lang").rows()
                assert len(got) == 2
                assert sorted(got.facets["meta.lang"]) == [("en", 1), ("tr", 1)]
                raw = await db.query(f"get {name} limit 1 facet kind top 1")
                assert raw.facets == {"kind": [FacetCount("lang", 2)]}

        asyncio.run(go())
    finally:
        client.query(f"drop collection if exists {name}")
