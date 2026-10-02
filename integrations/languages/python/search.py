# Python, through the fenecdb client (pip install fenecdb): the standard
# library alone, one statement a request. Run by ../run-tests.sh.
import os

from fenecdb import Client, FenecError

db = Client(os.environ.get("FENEC_URL", "http://127.0.0.1:8080"),
            token=os.environ.get("FENEC_TOKEN"))

db.query("create collection if not exists docs "
         "(title text, embed vector<3> @hnsw(cosine))")
db.query("put docs {title: $1, embed: $2}", ["Night at the oasis", [0.1, 0.2, 0.3]])
db.query("put docs {title: $1, embed: $2}", ["Dunes", [0.9, 0.1, 0.0]])

rows = db.query("get docs select title near embed $1 limit 5", [[0.1, 0.2, 0.3]])
assert [r["title"] for r in rows] == ["Night at the oasis", "Dunes"], rows

# The same through the query builder, which writes that statement itself.
docs = db.collection("docs")
query = docs.select("title").near("embed", [0.1, 0.2, 0.3]).limit(5)
assert query.to_fenecql() == ("get docs select title near embed $1 limit 5", [[0.1, 0.2, 0.3]])
assert query.rows() == rows, query.rows()
assert docs.where("title", "~", "Dunes").count() == 1

# A refusal is an exception carrying the server's message and status.
try:
    db.query("get nowhere")
    raise AssertionError("a missing collection was answered")
except FenecError as e:
    assert e.status == 404, e

print("python: ok")
