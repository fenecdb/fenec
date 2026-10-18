// C# with the FenecDb SDK. Run by ../run-tests.sh.
using System.Text.Json.Serialization;
using FenecDb;

using var db = new FenecClient(
    Environment.GetEnvironmentVariable("FENEC_URL") ?? "http://127.0.0.1:8080",
    new() { Token = Environment.GetEnvironmentVariable("FENEC_TOKEN") });

await db.ExecAsync("create collection if not exists docs (title text @text, year int @sorted)");
await db.ExecAsync("put docs {title: $1, year: $2}", ["Dunes", 2021]);
await db.ExecAsync("put docs {title: $1, year: $2}", ["Night at the oasis", 2024]);

var rows = await db.QueryAsync<Doc>("get docs select title, year where year >= $1 order year desc limit $2", [2020, 5]);
if (!rows.SequenceEqual([new Doc("Night at the oasis", 2024), new Doc("Dunes", 2021)]))
    throw new Exception($"the filtered read answered {string.Join(", ", rows)}");
var hits = await db.QueryAsync<Hit>("get docs select title match title $1 fuzzy", ["oasys"]);
if (hits.Count != 1 || hits[0].Title != "Night at the oasis" || hits[0].Score <= 0)
    throw new Exception($"match answered {string.Join(", ", hits)}");

// The same through the query builder, which writes that statement itself.
var docs = db.From("docs");
var query = docs.Select("title", "year").Where("year", ">=", 2020).Order("year", "desc").Limit(5);
if (query.ToFenecQL().Text != "get docs select title, year where year >= $1 order year desc limit $2")
    throw new Exception($"the builder wrote {query.ToFenecQL().Text}");
var built = await query.RowsAsync<Doc>();
if (!built.SequenceEqual(rows))
    throw new Exception($"the builder's query answered {string.Join(", ", built)}");
var found = await docs.Select("title").Match("title", "oasys", fuzzy: 2).RowsAsync<Hit>();
if (!found.SequenceEqual(hits))
    throw new Exception($"the builder's match answered {string.Join(", ", found)}");
if (await docs.Where("title", "~", "Dunes").CountAsync() != 1)
    throw new Exception("count answered another number");

try
{
    await db.QueryAsync("get nowhere");
    throw new Exception("a missing collection was answered");
}
catch (FenecException e) when (e.Status == 404) { }

Console.WriteLine(".NET (sdk): ok");

record Doc(string Title, int Year);
record Hit(string Title, [property: JsonPropertyName("_score")] float Score);
