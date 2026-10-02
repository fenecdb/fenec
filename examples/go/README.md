# Notes -- Go (server)

A notes CLI over a `fenec-server`, with the Go SDK
(`github.com/fenecdb/fenec/integrations/go`): the standard library alone,
every query sent over HTTP.

## What it shows

- the schema in `schema.fenecql` (embedded with `go:embed`), applied with
  `db.Schema(ctx, schema, nil, true)` at every start, and four notes seeded
  into an empty collection;
- creating notes through the query builder, and marking one done;
- full-text search with `Match`, fused with `Near` over a toy embedding
  (`Fuse`);
- filtering by tag (`tags has`) or by `done`, newest first, rows decoded
  into a struct with `fenecdb.RowsAs[Note]`;
- `watch`: a subscription to the open notes (server-sent events), the list
  printed again after every change.

## Prerequisites

Go 1.22 or newer, and `fenec-server`: a [release
binary](https://github.com/fenecdb/fenec/releases), or
`cargo build --release -p fenec-server` at the repository's root
(`target/release/fenec-server`).

## Run

```sh
fenec-server --file notes.fenec --http 127.0.0.1:8080 --http-token secret
```

```sh
cd examples/go
go mod tidy                              # fetches the SDK, writes go.sum
go run .                                 # seeds, then lists
go run . add "Dentist" "Call the dentist on Monday." health
go run . list --tag home
go run . list --open
go run . search istanbul trip            # match, then fuse
go run . done 1
go run . watch                           # Ctrl-C to stop; add a note from another terminal
go run . smoke                           # the whole tour, checked: what CI runs
```

`FENEC_URL` and `FENEC_TOKEN` point it at another server; `secret` is a
development default, never one to deploy.

## The core

```go
db := fenecdb.New(url, fenecdb.WithToken(token))
db.Schema(ctx, schema, nil, true)                       // made, or checked
db.From("notes").Insert(ctx, fenecdb.D("title", t, "body", b, "tags", []string{"home"},
	"done", false, "at", time.Now().UTC().Format(time.RFC3339), "embed", embed(t+" "+b)))

notes := db.From("notes").Select("id", "title", "tags", "done", "at")
fenecdb.RowsAs[Note](ctx, notes.Where("tags", "has", "work").Order("at", "desc"))
fenecdb.RowsAs[Note](ctx, notes.Match("body", words).Near("embed", embed(words)).Fuse().Limit(5))
db.From("notes").Where("id", "=", id).Update(ctx, map[string]any{"done": true})

events, _ := db.Subscribe(ctx, "notes", url.Values{"done": {"eq.false"}})   // seed, then changes
```

## The toy embedding

`embed()` is a placeholder, not a model: character trigrams (of the UTF-8
bytes) hashed into 64 dimensions with FNV-1a, so `near` and `fuse` have
something to rank without a download. It matches spelling, not meaning;
every Notes example computes the same vectors. A real app calls a model
here -- an embeddings API, or a local ONNX model -- and declares
`vector<N>` with that model's `N` in `schema.fenecql`.

## Against this repository's build

CI runs the example on the SDK in this checkout, through a `replace`
written into a copy of `go.mod`:

```sh
cd examples/go
go mod edit -replace github.com/fenecdb/fenec/integrations/go=../../integrations/go
go run . smoke
```

`examples/run-tests.sh go` does that in a copy of the folder, against a
`fenec-server` it builds and starts.
