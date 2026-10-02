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

- `Query` gives rows as `[]map[string]any`, `QueryAs[T]` decodes them by `json` tags; `Exec` a write's count and its `Seq`.
- `Batch(ctx, fenecdb.Stmt(q, params...), ...)` runs statements as one block: all land, or none.
- `IdempotencyKey(key)` makes a write once; `After(seq)` reads a write on a replica (`Fenec-After`).
- `Subscribe(ctx, collection, url.Values{"year": {"gte.2024"}})` is a channel of `seed` and `change` events (SSE).
- `Changes(ctx, since, wait)` reads every write on disk (`/_changes`); `Health(ctx)` asks `/_health`.
- `WithTenant("acme")` sends everything under `/t/acme/`; `WithTimeout`, `WithHTTPClient` as they say.
- A refusal is an `*fenecdb.Error` with `Status`, `Code` and `Message`, for `errors.As`.
- A `[]float32` goes out as the decimals that read back as each `f32`, so a vector round-trips to the bit.

`go test` runs against `fenec-server` processes it starts (`cargo build -p fenec-server`, or `FENEC_SERVER`).
Full reference: https://fenecdb.com/docs/languages
