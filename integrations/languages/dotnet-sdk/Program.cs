// C# with the FenecDb SDK. Run by ../run-tests.sh.
using System.Text.Json.Serialization;
using FenecDb;

using var db = new FenecClient(
    Environment.GetEnvironmentVariable("FENEC_URL") ?? "http://127.0.0.1:8080",
    new() { Token = Environment.GetEnvironmentVariable("FENEC_TOKEN") });

await db.ExecAsync("create collection if not exists docs (title text, embed vector<3> @hnsw(cosine))");
await db.ExecAsync("put docs {title: $1, embed: $2}", ["Night at the oasis", new[] { 0.1f, 0.2f, 0.3f }]);
await db.ExecAsync("put docs {title: $1, embed: $2}", ["Dunes", new[] { 0.9f, 0.1f, 0.0f }]);

var hits = await db.QueryAsync<Hit>("get docs select title near embed $1 limit 5", [new[] { 0.1f, 0.2f, 0.3f }]);
var titles = hits.Select(h => h.Title).ToArray();
if (!titles.SequenceEqual(["Night at the oasis", "Dunes"]))
    throw new Exception($"near answered {string.Join(", ", titles)}");

// The same through the query builder, which writes that statement itself.
var query = db.From("docs").Select("title").Near("embed", new[] { 0.1f, 0.2f, 0.3f }).Limit(5);
if (query.ToFenecQL().Text != "get docs select title near embed $1 limit 5")
    throw new Exception($"the builder wrote {query.ToFenecQL().Text}");
var built = await query.RowsAsync<Hit>();
if (!built.SequenceEqual(hits))
    throw new Exception($"the builder's query answered {string.Join(", ", built)}");
if (await db.From("docs").Where("title", "~", "Dunes").CountAsync() != 1)
    throw new Exception("count answered another number");

try
{
    await db.QueryAsync("get nowhere");
    throw new Exception("a missing collection was answered");
}
catch (FenecException e) when (e.Status == 404) { }

Console.WriteLine(".NET (sdk): ok");

record Hit(string Title, [property: JsonPropertyName("_score")] float Score);
