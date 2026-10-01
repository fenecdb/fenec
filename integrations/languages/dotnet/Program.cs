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

await Query("create collection if not exists docs (title text, embed vector<3> @hnsw(cosine))");
await Query("put docs {title: $1, embed: $2}", "Night at the oasis", new[] { 0.1f, 0.2f, 0.3f });
await Query("put docs {title: $1, embed: $2}", "Dunes", new[] { 0.9f, 0.1f, 0.0f });

var rows = await Query("get docs select title near embed $1 limit 5", new[] { 0.1f, 0.2f, 0.3f });
var titles = rows.EnumerateArray().Select(r => r.GetProperty("title").GetString()).ToArray();
if (!titles.SequenceEqual(new[] { "Night at the oasis", "Dunes" }))
    throw new Exception($"near answered {string.Join(", ", titles)}");

try
{
    await Query("get nowhere");
    throw new Exception("a missing collection was answered");
}
catch (HttpRequestException e) when (e.StatusCode == HttpStatusCode.NotFound) { }

Console.WriteLine(".NET: ok");
