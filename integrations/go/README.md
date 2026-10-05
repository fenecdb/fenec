# fenecdb for Go

A client for fenec-server's HTTP endpoint, the standard library alone.

```sh
go get github.com/fenecdb/fenec/integrations/go
```

```go
import fenecdb "github.com/fenecdb/fenec/integrations/go"

db := fenecdb.New("http://127.0.0.1:8080", fenecdb.WithToken("secret"))
w, err := db.Exec(ctx, "put docs {title: $1, embed: $2}", "Dunes", []float32{0.9, 0.1, 0})
rows, err := db.Query(ctx, "get docs select title near embed $1 limit 5", []float32{0.1, 0.2, 0.3})
hits, err := fenecdb.QueryAs[Hit](ctx, db, "get docs select title near embed $1 limit 5", vec)
```

The query builder writes the statement -- every value a parameter, every name checked -- the same text the
Python, JavaScript and .NET builders make of the same chain. A `*Builder` is immutable, each step a copy, and the
first refused step comes back from whatever ends the chain:

```go
docs := db.From("articles")
q := docs.Select("title").
	Where("year", ">=", 2024).
	WhereCond(fenecdb.Or(fenecdb.Fields("lang", "tr"), fenecdb.Fields("tags", fenecdb.Ops("has", "rust")))).
	Near("embed", vec, fenecdb.Ef(64)).
	Limit(5)
rows, err := q.Rows(ctx)                       // or fenecdb.RowsAs[Hit](ctx, q), q.First(ctx), q.Count(ctx)
docs.Insert(ctx, fenecdb.D("title", "Dunes", "year", 2021))
docs.Where("year", "<", 2000).Delete(ctx)      // no filter: refused unless fenecdb.All()
```

- `Query` gives rows as `[]map[string]any`, `QueryAs[T]` decodes them by `json` tags; `Exec` a write's count and its `Seq`.
- `Batch(ctx, fenecdb.Stmt(q, params...), ...)` runs statements as one block: all land, or none.
- `IdempotencyKey(key)` makes a write once; `After(seq)` reads a write on a replica (`Fenec-After`).
- `Subscribe(ctx, collection, url.Values{"year": {"gte.2024"}})` is a channel of `seed` and `change` events (SSE).
- `Changes(ctx, since, wait)` reads every write on disk (`/_changes`); `Health(ctx)` asks `/_health`.
- `WithTenant("acme")` sends everything under `/t/acme/`; `WithTimeout`, `WithHTTPClient` as they say.
- A refusal is an `*fenecdb.Error` with `Status`, `Code` and `Message`, for `errors.As`; a failed `Batch`'s also
  says which statement stopped it, `At` (from 0; -1 otherwise), and how many stayed applied, `Completed`.
- A `[]float32` goes out as the decimals that read back as each `f32`, so a vector round-trips to the bit.

- The builder's options are functional: `Ef`, `Exact`, `K`, `Candidates`, `Collate`, `All`, and `On`, `ParentKey`,
  `Select`, `Where`, `Required`, `Sort`, `Limit`, `Offset` for `Lookup`. An object condition is `Fields` and `Ops`,
  names and values in turn, and a document a `D(...)` in its order, a map in its sorted keys, or a struct.
- `Facet(field, Ranges(0, 25, 50))` counts by ranges of numbers, each value `[from, to]`; `Disjunctive()` counts past
  the filter's own condition on the field, as a shop's filter list does.
- Aggregates go in `Select` as FenecQL spells them (`"count(*)"`, `"sum(total)"`), and `Group` takes one key or
  more; `Bucket(field, "1m")`, `CountDistinct`, `First(field, by)`, `Last` and `Expr` make a column, named with
  `.As(name)`: `Select(fenecdb.Bucket("at", "1h").As("hour"), "count(*)").Group("hour")`.
- `Highlight(field, Tags(pre, post))` and `Snippet(field, words, Ellipsis("…"))` answer a `match`'s marks under
  `highlight(field)` and `snippet(field)`; `Facet(field, Top(n))` counts values over every matched row, which
  `q.Answer(ctx)` (or `db.QueryAnswer`, a `BatchItem`'s `Facets`) hands back beside the rows, `Facets.Of(field)`.
- To see the text and parameters a chain builds, for logging or a test, `q.ToFenecQL()` returns them and runs
  nothing; a query needs no call to it before it runs.

`go test` runs against `fenec-server` processes it starts (`cargo build -p fenec-server`, or `FENEC_SERVER`), and the
builder against every case of `integrations/builder-golden.json`.
Full reference: https://fenecdb.com/docs/languages
