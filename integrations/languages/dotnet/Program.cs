// C# with HttpClient and System.Net.Http.Json alone. Run by ../run-tests.sh.
using System.Net;
using System.Net.Http.Headers;
using System.Net.Http.Json;
using System.Text;
using System.Text.Json;

var http = new HttpClient
{
    BaseAddress = new Uri(Environment.GetEnvironmentVariable("FENEC_URL") ?? "http://127.0.0.1:8080"),
};
var token = Environment.GetEnvironmentVariable("FENEC_TOKEN");
if (token is not null)
    http.DefaultRequestHeaders.Authorization = new AuthenticationHeaderValue("Bearer", token);

// One FenecQL statement; a refusal throws with the server's status and message.
async Task<JsonElement> Query(string query, params object[] args)
{
    // Serialised whole rather than PostAsJsonAsync's stream: the server wants
    // a Content-Length and takes no chunked body.
    var json = JsonSerializer.Serialize(new { query, @params = args });
    var res = await http.PostAsync("/query", new StringContent(json, Encoding.UTF8, "application/json"));
    var body = await res.Content.ReadFromJsonAsync<JsonElement>();
    if (!res.IsSuccessStatusCode)
        throw new HttpRequestException(body.GetProperty("error").GetString(), null, res.StatusCode);
    return body;
}

await Query("create collection if not exists docs (title text @text, year int @sorted)");
await Query("put docs {title: $1, year: $2}", "Dunes", 2021);
await Query("put docs {title: $1, year: $2}", "Night at the oasis", 2024);

var rows = await Query("get docs select title, year where year >= $1 order year desc limit $2", 2020, 5);
var got = rows.EnumerateArray()
    .Select(r => $"{r.GetProperty("title").GetString()} {r.GetProperty("year").GetInt32()}").ToArray();
if (!got.SequenceEqual(new[] { "Night at the oasis 2024", "Dunes 2021" }))
    throw new Exception($"the filtered read answered {string.Join(", ", got)}");

var hits = await Query("get docs select title match title $1 fuzzy", "oasys");
var titles = hits.EnumerateArray().Select(r => r.GetProperty("title").GetString()).ToArray();
if (!titles.SequenceEqual(new[] { "Night at the oasis" }) || hits[0].GetProperty("_score").GetSingle() <= 0)
    throw new Exception($"match answered {hits}");

try
{
    await Query("get nowhere");
    throw new Exception("a missing collection was answered");
}
catch (HttpRequestException e) when (e.StatusCode == HttpStatusCode.NotFound) { }

Console.WriteLine(".NET: ok");
