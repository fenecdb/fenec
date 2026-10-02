# Notes -- .NET (server)

A notes console app over a `fenec-server`, with the `FenecDb` package:
`HttpClient` and `System.Text.Json` alone, every query sent over HTTP.

## What it shows

- the schema in `schema.fenecql`, applied with
  `db.SchemaAsync(text, migrate: true)` at every start, and four notes
  seeded into an empty collection;
- creating notes through the query builder, and marking one done;
- full-text search with `Match`, fused with `Near` over a toy embedding
  (`Fuse`);
- filtering by tag (`tags has`) or by `done`, newest first, rows mapped onto
  a `Note` record with `RowsAsync<Note>()`;
- `watch`: a subscription to the open notes (`SubscribeAsync`, an
  `IAsyncEnumerable`), the list printed again after every change.

## Prerequisites

The .NET 8 SDK or newer, and `fenec-server`: a [release
binary](https://github.com/fenecdb/fenec/releases), or
`cargo build --release -p fenec-server` at the repository's root
(`target/release/fenec-server`).

## Run

```sh
fenec-server --file notes.fenec --http 127.0.0.1:8080 --http-token secret
```

```sh
cd examples/dotnet
dotnet run                               # seeds, then lists
dotnet run -- add "Dentist" "Call the dentist on Monday." health
dotnet run -- list --tag home
dotnet run -- list --open
dotnet run -- search istanbul trip       # match, then fuse
dotnet run -- done 1
dotnet run -- watch                      # Ctrl-C to stop; add a note from another terminal
dotnet run -- smoke                      # the whole tour, checked: what CI runs
```

`FENEC_URL` and `FENEC_TOKEN` point it at another server; `secret` is a
development default, never one to deploy.

## The core

```csharp
using var db = new FenecClient(url, new() { Token = token });
await db.SchemaAsync(File.ReadAllText("schema.fenecql"), migrate: true);   // made, or checked
await db.From("notes").InsertAsync(new { title, body, tags, done = false,
    at = DateTime.UtcNow.ToString("o"), embed = Embed($"{title} {body}") });

var notes = db.From("notes").Select("id", "title", "tags", "done", "at");
await notes.Where("tags", "has", "work").Order("at", "desc").RowsAsync<Note>();
await notes.Match("body", words).Near("embed", Embed(words)).Fuse().Limit(5).RowsAsync<Note>();
await db.From("notes").Where("id", "=", id).UpdateAsync(new { done = true });

await foreach (var ev in db.SubscribeAsync("notes", [new("done", "eq.false")])) { /* seed, then changes */ }
```

## The toy embedding

`Notes.Embed` is a placeholder, not a model: character trigrams (of the
UTF-8 bytes) hashed into 64 dimensions with FNV-1a, so `near` and `fuse`
have something to rank without a download. It matches spelling, not
meaning; every Notes example computes the same vectors. A real app calls a
model here -- an embeddings API, or a local ONNX model -- and declares
`vector<N>` with that model's `N` in `schema.fenecql`.

## Against this repository's build

`Notes.csproj` takes the NuGet package unless `FenecLocal` names a checkout
of this repository; then it references the SDK's project there, as CI does:

```sh
cd examples/dotnet
dotnet run -p:FenecLocal=$PWD/../.. -- smoke
```
