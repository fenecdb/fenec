# FenecDb for .NET

A client for fenec-server's HTTP endpoint, `HttpClient` and `System.Text.Json` alone, for .NET 8 and newer.

```sh
dotnet add package FenecDb
```

```csharp
using FenecDb;

using var db = new FenecClient("http://127.0.0.1:8080", new() { Token = "secret" });
var w = await db.ExecAsync("put docs {title: $1, embed: $2}", ["Dunes", new[] { 0.9f, 0.1f, 0f }]);
var rows = await db.QueryAsync("get docs select title near embed $1 limit 5", [new[] { 0.1f, 0.2f, 0.3f }]);
var hits = await db.QueryAsync<Hit>("get docs select title near embed $1 limit 5", [vector]);
```

The query builder writes the statement -- every value a parameter, every name checked -- the same text the
Python, JavaScript and Go builders make of the same chain. A `Query` is immutable, each call a new one:

```csharp
var docs = db.From("articles");
var q = docs.Select("title")
    .Where("year", ">=", 2024)
    .Where(Cond.Or(Cond.Cmp("lang", "=", "tr"), Cond.Cmp("tags", "has", "rust")))
    .Near("embed", vector, ef: 64)
    .Limit(5);
var hits = await q.RowsAsync<Hit>();           // or RowsAsync(), FirstAsync(), CountAsync()
await docs.InsertAsync(new { title = "Dunes", year = 2021 });
await docs.Where("year", "<", 2000).DeleteAsync();   // no filter: refused unless all: true
await docs.Lookup("reviews", on: "article_id", limit: 3, order: [new("created", "desc")]).RowsAsync();
```

- `QueryAsync` gives rows as `JsonElement`s, `QueryAsync<T>` maps them to records or classes by property name, any case; `ExecAsync` a write's count and its `Seq`.
- `BatchAsync([new Statement(q, params), ...])` runs statements as one block: all land, or none.
- `WithIdempotencyKey(key)` makes a write once; `After(seq)` reads a write on a replica (`Fenec-After`).
- `SubscribeAsync(collection, [new("year", "gte.2024")])` is an `IAsyncEnumerable<Event>` of `seed` and `change` events (SSE).
- `ChangesAsync(since, wait)` reads every write on disk (`/_changes`); `HealthAsync()` asks `/_health`.
- `FenecClientOptions` takes `Token`, `Tenant` (`/t/<tenant>/`), `Timeout` and an `HttpClient` of your own.
- A refusal is a `FenecException` with `Status`, `Code` and the server's message; a failed batch's also says which
  statement stopped it, `At` (from 0; `null` otherwise), and how many stayed applied, `Completed`. Every call takes
  a `CancellationToken`.
- A `float[]` or `ReadOnlyMemory<float>` goes out as the decimals that read back as each float, so a vector round-trips to the bit.

- `Facet(field, ranges: [0, 25, 50])` counts by ranges of numbers, each value `[from, to]`; `disjunctive: true`
  counts past the filter's own condition on the field, as a shop's filter list does.
- A `Dictionary<string, object?>` is the builder's object condition, its operators a dictionary too; a refused
  step throws `FenecQueryException` with the JS builder's message.
- Aggregates go in `Select` as FenecQL spells them (`"count(*)"`, `"sum(total)"`), and `Group` takes one key or
  more; `Computed.Bucket(field, "1m")`, `CountDistinct`, `First(field, by)`, `Last` and `Expr` make a column,
  named with `.As(name)`: `Select(Computed.Bucket("at", "1h").As("hour"), "count(*)").Group("hour")`.
- To see the text and parameters a chain builds, for logging or a test, `q.ToFenecQL()` returns them and runs
  nothing; a query needs no call to it before it runs.

`dotnet test FenecDb.Tests` runs against `fenec-server` processes it starts (`cargo build -p fenec-server`, or
`FENEC_SERVER`), and the builder against every case of `integrations/builder-golden.json`.
Full reference: https://fenecdb.com/docs/languages
