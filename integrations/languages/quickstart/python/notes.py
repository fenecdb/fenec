from fenecdb import Client, FenecError

db = Client("http://127.0.0.1:8080")

db.query("put notes {title: $1, year: $2, kind: $3}", ["Sand and stars", 2023, "book"])

rows = db.query("get notes where year >= $1 order year desc", [2020])
print("from 2020:", ", ".join(r["title"] for r in rows))

hits = db.query("get notes select title match title $1 fuzzy", ["oasys"])
print("search:", ", ".join(h["title"] for h in hits))

try:
    db.query("get nowhere")
except FenecError as e:
    print("refused:", e.status, e)
