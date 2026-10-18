# Python, through the fenecdb client (pip install fenecdb): the standard
# library alone, one statement a request. Run by ../run-tests.sh.
import os

from fenecdb import Client, FenecError

db = Client(os.environ.get("FENEC_URL", "http://127.0.0.1:8080"),
            token=os.environ.get("FENEC_TOKEN"))

db.query("create collection if not exists docs "
         "(title text @text, year int @sorted)")
db.query("put docs {title: $1, year: $2}", ["Dunes", 2021])
db.query("put docs {title: $1, year: $2}", ["Night at the oasis", 2024])

rows = db.query("get docs select title, year where year >= $1 order year desc limit $2", [2020, 5])
assert rows == [{"title": "Night at the oasis", "year": 2024},
                {"title": "Dunes", "year": 2021}], rows

hits = db.query("get docs select title match title $1 fuzzy", ["oasys"])
assert [h["title"] for h in hits] == ["Night at the oasis"], hits
assert hits[0]["_score"] > 0, hits

# The same through the query builder, which writes that statement itself.
docs = db.collection("docs")
query = docs.select("title", "year").where("year", ">=", 2020).order("year", "desc").limit(5)
assert query.to_fenecql() == (
    "get docs select title, year where year >= $1 order year desc limit $2", [2020, 5])
assert query.rows() == rows, query.rows()
assert docs.select("title").match("title", "oasys", fuzzy=True).rows() == hits
assert docs.where("title", "~", "Dunes").count() == 1

# A refusal is an exception carrying the server's message and status.
try:
    db.query("get nowhere")
    raise AssertionError("a missing collection was answered")
except FenecError as e:
    assert e.status == 404, e

print("python: ok")
