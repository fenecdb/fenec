using System.Globalization;
using System.Net;
using System.Text;
using System.Text.Json;
using Xunit;

namespace FenecDb.Tests;

/// <summary>
/// The query builder: held to integrations/builder-golden.json -- the text and parameters the JavaScript builder
/// makes of every chain there -- and run against the primary, where each answer is the answer to the same
/// statement written by hand.
/// </summary>
[Collection("servers")]
public sealed class BuilderTests(Servers servers)
{
    static readonly Lazy<Dictionary<string, JsonElement>> Golden = new(() =>
    {
        // run-tests.sh mounts the file into its container, which holds no repository.
        var path = Environment.GetEnvironmentVariable("FENEC_GOLDEN")
            ?? Path.Combine(Servers.RepoRoot(), "integrations", "builder-golden.json");
        using var doc = JsonDocument.Parse(File.ReadAllText(path));
        return doc.RootElement.EnumerateArray().ToDictionary(c => c.GetProperty("name").GetString()!, c => c.Clone());
    });

    public static IEnumerable<object[]> Cases() => Golden.Value.Keys.Select(k => new object[] { k });

    // ------------------------------------------------------------- the steps

    /// <summary>An argument as a C# caller would hand it over.</summary>
    static object? Value(JsonElement e) => e.ValueKind switch
    {
        JsonValueKind.String => e.GetString(),
        JsonValueKind.True => true,
        JsonValueKind.False => false,
        JsonValueKind.Null => null,
        JsonValueKind.Number => e.GetRawText().IndexOfAny(['.', 'e', 'E']) < 0 ? e.GetInt64() : e.GetDouble(),
        JsonValueKind.Array => e.EnumerateArray().Select(Value).ToList(),
        _ when e.TryGetProperty("$date", out var d) => DateTimeOffset.Parse(d.GetString()!, CultureInfo.InvariantCulture),
        _ when e.TryGetProperty("$f32", out var f) => f.EnumerateArray().Select(x => x.GetSingle()).ToArray(),
        _ when e.TryGetProperty("$inc", out var n) => Computed.Inc(Value(n)),
        _ when e.TryGetProperty("$expr", out var x) =>
            Computed.Expr(x[0].GetString()!, x.EnumerateArray().Skip(1).Select(Value).ToArray()),
        _ => e.EnumerateObject().ToDictionary(p => p.Name, p => Value(p.Value)),
    };

    static bool Plain(JsonElement e) =>
        e.ValueKind == JsonValueKind.Object && !e.EnumerateObject().Any(p => p.Name.StartsWith('$'));

    /// <summary>What a field is held to in an object condition: an operator dictionary, or a value.</summary>
    static object? Spec(JsonElement e) => Plain(e)
        ? e.EnumerateObject().ToDictionary(p => p.Name, p => p.Name == "not" ? Spec(p.Value) : Value(p.Value))
        : Value(e);

    static Cond Condition(JsonElement e)
    {
        if (e.TryGetProperty("$or", out var or)) return Cond.Or(or.EnumerateArray().Select(Condition).ToArray());
        if (e.TryGetProperty("$and", out var and)) return Cond.And(and.EnumerateArray().Select(Condition).ToArray());
        if (e.TryGetProperty("$not", out var not)) return Cond.Not(Condition(not));
        if (e.TryGetProperty("$raw", out var raw))
        {
            var list = raw.EnumerateArray().ToList();
            return Cond.Raw(list[0].GetString()!, list.Skip(1).Select(Value).ToArray());
        }
        return e.EnumerateObject().ToDictionary(p => p.Name, p => Spec(p.Value));
    }

    static JsonElement? Opt(JsonElement[] args, int at, string name) =>
        args.Length > at && args[at].TryGetProperty(name, out var v) ? v : null;

    static long? Long(JsonElement? e) => e?.GetInt64();

    static IEnumerable<SortKey>? Order(JsonElement? e) => e switch
    {
        null => null,
        { ValueKind: JsonValueKind.String } s => [s.GetString()!],
        { } list => list.EnumerateArray().Select(k => k.ValueKind == JsonValueKind.String
            ? new SortKey(k.GetString()!)
            : new SortKey(k[0].GetString()!, k.GetArrayLength() > 1 ? k[1].GetString()! : "asc",
                k.GetArrayLength() > 2 && k[2].TryGetProperty("collate", out var c) ? c.GetString() : null)).ToList(),
    };

    static object Docs(JsonElement e) => e.ValueKind == JsonValueKind.Array
        ? e.EnumerateArray().Select(d => (object)Value(d)!).ToList()
        : Value(e)!;

    static Query Step(Query q, string op, JsonElement[] a) => op switch
    {
        "select" => q.Select(a.Select(x => x.GetString()!).ToArray()),
        "where" when a.Length == 3 => q.Where(a[0].GetString()!, a[1].GetString()!, Value(a[2])),
        "where" when a.Length == 2 => q.Where(a[0].GetString()!, Spec(a[1])),
        "where" => q.Where(Condition(a[0])),
        "orWhere" when a.Length == 3 => q.OrWhere(a[0].GetString()!, a[1].GetString()!, Value(a[2])),
        "orWhere" when a.Length == 2 => q.OrWhere(a[0].GetString()!, Spec(a[1])),
        "orWhere" => q.OrWhere(Condition(a[0])),
        "near" => q.Near(a[0].GetString()!, Value(a[1]), Long(Opt(a, 2, "ef")), Opt(a, 2, "exact")?.GetBoolean() ?? false),
        "rerank" => q.Rerank(a[0].GetString()!, Value(a[1]), Long(Opt(a, 2, "candidates"))),
        "match" => q.Match(a[0].GetString()!, a[1].GetString()!),
        "fuse" => q.Fuse(Long(Opt(a, 0, "k")), Long(Opt(a, 0, "candidates"))),
        "group" => q.Group(a[0].GetString()!),
        "order" => q.Order(a[0].GetString()!, a.Length > 1 ? a[1].GetString()! : "asc", Opt(a, 2, "collate")?.GetString()),
        "limit" => q.Limit(a[0].GetInt64()),
        "offset" => q.Offset(a[0].GetInt64()),
        "lookup" => q.Lookup(a[0].GetString()!,
            on: Opt(a, 1, "on")?.GetString(),
            parentKey: Opt(a, 1, "parentKey")?.GetString(),
            select: Opt(a, 1, "select") switch
            {
                null => null,
                { ValueKind: JsonValueKind.String } s => [s.GetString()!],
                { } list => list.EnumerateArray().Select(x => x.GetString()!).ToList(),
            },
            where: Opt(a, 1, "where") is { } w ? Condition(w) : null,
            required: Opt(a, 1, "required")?.GetBoolean() ?? false,
            order: Order(Opt(a, 1, "order")),
            limit: Long(Opt(a, 1, "limit")),
            offset: Long(Opt(a, 1, "offset"))),
        "highlight" when Texts(a, 1, "pre", "post") =>
            q.Highlight(a[0].GetString()!, Opt(a, 1, "pre")?.GetString(), Opt(a, 1, "post")?.GetString()),
        "highlight" => q.Highlighted(a[0].GetString()!, Arg(a, 1, "pre"), Arg(a, 1, "post")),
        "snippet" when Texts(a, 2, "ellipsis", "pre", "post") =>
            q.Snippet(a[0].GetString()!, a[1].GetInt64(), Opt(a, 2, "ellipsis")?.GetString(),
                Opt(a, 2, "pre")?.GetString(), Opt(a, 2, "post")?.GetString()),
        "snippet" => q.Snipped(a[0].GetString()!, a[1].GetInt64(), Arg(a, 2, "ellipsis"), Arg(a, 2, "pre"), Arg(a, 2, "post")),
        "facet" => q.Facet(a[0].GetString()!, Long(Opt(a, 1, "top"))),
        _ => throw new InvalidOperationException($"no builder step {op}"),
    };

    // Whether a step's options are text where given, which the typed step takes; a chain handing a number for a
    // tag goes through the step's untyped path, the one a C# caller cannot reach, to be refused as JS refuses it.
    static bool Texts(JsonElement[] a, int at, params string[] names) =>
        names.All(n => Opt(a, at, n) is not { } v || v.ValueKind == JsonValueKind.String);

    static object? Arg(JsonElement[] a, int at, string name) => Opt(a, at, name) is { } v ? Value(v) : null;

    /// <summary>A server that answers as fenec-server would and keeps the last body it was sent.</summary>
    sealed class Recorder : HttpMessageHandler
    {
        public string? Body;

        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            Body = await request.Content!.ReadAsStringAsync(ct);
            var query = JsonDocument.Parse(Body).RootElement.GetProperty("query").GetString()!;
            var rows = query.EndsWith(" count") || query.Contains(" count facet ") ? """[{"count":0}]""" : "[]";
            // A query asking facets is answered with them beside the rows.
            var answer = query.Split(' ')[0] is "put" or "set" or "del" ? """{"affected":0}"""
                : query.Contains(" facet ") ? """{"rows":""" + rows + ""","facets":{}}""" : rows;
            return new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent(answer, Encoding.UTF8, "application/json") };
        }
    }

    static async Task<(string? Text, JsonElement Params, string? Error)> Run(JsonElement steps)
    {
        var list = steps.EnumerateArray().ToList();
        var recorder = new Recorder();
        using var db = new FenecClient("http://golden.invalid", new() { HttpClient = new HttpClient(recorder) });
        static JsonElement[] Args(JsonElement s) => s.TryGetProperty("args", out var a) ? a.EnumerateArray().ToArray() : [];
        try
        {
            var q = db.From(Args(list[0])[0].GetString()!);
            for (var i = 1; i < list.Count; i++)
            {
                var op = list[i].GetProperty("op").GetString()!;
                var a = Args(list[i]);
                (string, IReadOnlyList<object?>)? text = op switch
                {
                    "toFenecQL" => q.ToFenecQL(),
                    "toInsert" => q.ToInsert(Docs(a[0]), Opt(a, 1, "ifAbsent")?.GetBoolean() ?? false, Long(Opt(a, 1, "require"))),
                    "toUpdate" => q.ToUpdate(Value(a[0])!, Opt(a, 1, "all")?.GetBoolean() ?? false, Long(Opt(a, 1, "require"))),
                    "toDelete" => q.ToDelete(Opt(a, 0, "all")?.GetBoolean() ?? false, Long(Opt(a, 0, "require"))),
                    _ => null,
                };
                if (text is var (t, ps))
                {
                    // The parameters as the client writes them.
                    using var buf = new MemoryStream();
                    Params.WriteBody(buf, t, ps);
                    return (t, JsonDocument.Parse(buf.ToArray()).RootElement.GetProperty("params").Clone(), null);
                }
                Task? sent = op switch
                {
                    "rows" => q.RowsAsync(),
                    "first" => q.FirstAsync(),
                    "count" => q.CountAsync(),
                    "explain" => q.ExplainAsync(),
                    "insert" => q.InsertAsync(Docs(a[0]), Opt(a, 1, "ifAbsent")?.GetBoolean() ?? false, Long(Opt(a, 1, "require"))),
                    "update" => q.UpdateAsync(Value(a[0])!, Opt(a, 1, "all")?.GetBoolean() ?? false, Long(Opt(a, 1, "require"))),
                    "delete" => q.DeleteAsync(Opt(a, 0, "all")?.GetBoolean() ?? false, Long(Opt(a, 0, "require"))),
                    _ => null,
                };
                if (sent is not null)
                {
                    await sent;
                    var body = JsonDocument.Parse(recorder.Body!).RootElement;
                    return (body.GetProperty("query").GetString(), body.GetProperty("params").Clone(), null);
                }
                q = Step(q, op, a);
            }
            throw new InvalidOperationException("a chain ends with a statement");
        }
        catch (FenecQueryException e)
        {
            return (null, default, e.Message);
        }
    }

    // JSON compared by value: a number the file writes as 1 and the client as 1.0 is one number to the server.
    static bool Same(JsonElement a, JsonElement b) => (a.ValueKind, b.ValueKind) switch
    {
        (JsonValueKind.Number, JsonValueKind.Number) => a.GetDouble() == b.GetDouble(),
        (JsonValueKind.String, JsonValueKind.String) => a.GetString() == b.GetString(),
        (JsonValueKind.Array, JsonValueKind.Array) => a.GetArrayLength() == b.GetArrayLength()
            && a.EnumerateArray().Zip(b.EnumerateArray()).All(p => Same(p.First, p.Second)),
        (JsonValueKind.Object, JsonValueKind.Object) => a.EnumerateObject().Count() == b.EnumerateObject().Count()
            && a.EnumerateObject().All(p => b.TryGetProperty(p.Name, out var v) && Same(p.Value, v)),
        var (x, y) when x == y => a.GetRawText() == b.GetRawText(),
        _ => false,
    };

    [Fact]
    public void TheGoldenFileHoldsEnoughCases()
    {
        Assert.True(Golden.Value.Count >= 60, $"{Golden.Value.Count} cases");
    }

    [Theory]
    [MemberData(nameof(Cases))]
    public async Task AGoldenCase(string name)
    {
        var c = Golden.Value[name];
        var (text, ps, error) = await Run(c.GetProperty("steps"));
        if (c.TryGetProperty("error", out var want))
        {
            Assert.Equal(want.GetString(), error ?? $"no refusal: {text}");
            return;
        }
        Assert.Null(error);
        Assert.Equal(c.GetProperty("text").GetString(), text);
        Assert.True(Same(c.GetProperty("params"), ps), $"params {ps.GetRawText()}, want {c.GetProperty("params").GetRawText()}");
    }

    [Fact]
    public async Task AQueryIsImmutableAndCanBeBranched()
    {
        var b = Query.From("articles").Where("year", ">=", 2024);
        var a = b.Where("tags", "has", "rust");
        var l = b.Limit(3);
        Assert.Equal("get articles where year >= $1", b.ToFenecQL().Text);
        Assert.Equal("get articles where year >= $1 and tags has $2", a.ToFenecQL().Text);
        Assert.Equal("get articles where year >= $1 limit 3", l.ToFenecQL().Text);
        Assert.Throws<FenecQueryException>(() => Query.From("t; drop collection x"));
        await Assert.ThrowsAsync<FenecQueryException>(() => Query.From("t").RowsAsync());
    }

    sealed record Note(string Title, [property: System.Text.Json.Serialization.JsonPropertyName("embed")] float[] Vector);

    [Fact]
    public void DocumentsAreDictionariesOrObjects()
    {
        var (text, ps) = Query.From("notes").ToInsert(new object[]
        {
            new Note("a", [0.5f]),
            new { title = "b", stars = 3 },
            new Dictionary<string, object?> { ["title"] = "c", ["at"] = new DateTime(2026, 1, 2, 3, 4, 5, 678, DateTimeKind.Utc) },
        });
        Assert.Equal("put notes [{Title: $1, embed: $2}, {title: $3, stars: $4}, {title: $5, at: $6}]", text);
        Assert.Equal("2026-01-02T03:04:05.678Z", ps[5]);
        Assert.Throws<FenecQueryException>(() => Query.From("notes").ToInsert(42));
    }

    // ---------------------------------------------------------- against the primary

    sealed record Title(string title);

    [Fact]
    public async Task BuilderAnswersAreTheTextsAnswers()
    {
        using var db = new FenecClient(servers.Primary, new FenecClientOptions { Token = Servers.RootToken });
        var name = $"shelf_{Environment.ProcessId}_{Guid.NewGuid():N}"[..30];
        var notes = $"notes_{Environment.ProcessId}_{Guid.NewGuid():N}"[..30];
        await db.ExecAsync($"create collection {name} (title text, year int @sorted, lang text @hash, tags [text], body text @text, embed vector<3> @hnsw(cosine))");
        await db.ExecAsync($"create collection {notes} (doc_id int @hash, stars int)");
        var shelf = db.From(name);
        var w = await shelf.InsertAsync(new object[]
        {
            new { title = "Night at the oasis", year = 2024, lang = "en", tags = new[] { "desert" }, body = "a night under the stars at the oasis", embed = new[] { 0.1f, 0.2f, 0.3f } },
            new { title = "Dunes", year = 2021, lang = "en", tags = new[] { "desert", "sand" }, body = "dunes move with the wind", embed = new[] { 0.9f, 0.1f, 0f } },
            new { title = "Kum", year = 2023, lang = "tr", tags = new[] { "sand" }, body = "kum ve rüzgar", embed = new[] { 0.2f, 0.8f, 0.1f } },
        });
        Assert.Equal(3, w.Affected);
        Assert.True(w.Seq > 0);
        await db.From(notes).InsertAsync(new[]
        {
            new Dictionary<string, object?> { ["doc_id"] = 1, ["stars"] = 5 },
            new Dictionary<string, object?> { ["doc_id"] = 1, ["stars"] = 3 },
            new Dictionary<string, object?> { ["doc_id"] = 3, ["stars"] = 4 },
        });

        float[] v = [0.1f, 0.2f, 0.3f];
        var pairs = new (Query Q, string Text, object?[] Params)[]
        {
            (shelf.Select("title").Where("year", ">=", 2022).Order("year", "desc"),
                $"get {name} select title where year >= $1 order year desc", [2022]),
            (shelf.Select("title").Where(new Dictionary<string, object?> { ["lang"] = "en", ["tags"] = new Dictionary<string, object?> { ["has"] = "sand" } }),
                $"get {name} select title where lang = $1 and tags has $2", ["en", "sand"]),
            (shelf.Select("title").Where(Cond.Or(Cond.Cmp("lang", "=", "tr"), Cond.Cmp("year", "<", 2022))).Order("title"),
                $"get {name} select title where lang = $1 or year < $2 order title asc", ["tr", 2022]),
            (shelf.Select("title").Near("embed", v).Limit(2), $"get {name} select title near embed $1 limit 2", [v]),
            (shelf.Select("title").Match("body", "oasis stars"), $"get {name} select title match body $1", ["oasis stars"]),
            (shelf.Select("title").Where("id", "in", new[] { 1, 3 }).Lookup(notes, on: "doc_id", select: ["stars"], order: [new("stars", "desc")]),
                $"get {name} select title where id in [$1, $2] lookup {notes} on doc_id select stars order stars desc", [1, 3]),
            (shelf.Select("lang", "count(*)").Group("lang").Order("lang"),
                $"get {name} select lang, count(*) group lang order lang asc", []),
        };
        foreach (var (q, text, ps) in pairs)
        {
            Assert.Equal(text, q.ToFenecQL().Text);
            var got = (await q.RowsAsync()).Select(r => r.GetRawText()).ToList();
            var want = (await db.QueryAsync(text, ps)).Select(r => r.GetRawText()).ToList();
            Assert.NotEmpty(got);
            Assert.Equal(want, got);
        }
        var titles = await shelf.Select("title").Order("year").RowsAsync<Title>();
        Assert.Equal("Dunes", titles[0].title);
        Assert.Equal(2, await shelf.Where("lang", "en").CountAsync());
        Assert.Equal("Dunes", (await shelf.Order("year").FirstAsync())!.Value.GetProperty("title").GetString());
        Assert.Equal("Dunes", (await shelf.Order("year").FirstAsync<Title>())!.title);
        Assert.Null(await shelf.Where("lang", "xx").FirstAsync());
        Assert.NotEmpty(await shelf.Near("embed", v).Limit(1).ExplainAsync());

        Assert.Equal(1, (await shelf.Where("lang", "tr").UpdateAsync(new { year = 2025 })).Affected);
        await Assert.ThrowsAsync<FenecQueryException>(() => shelf.DeleteAsync());
        Assert.Equal(1, (await shelf.Where("year", "<", 2022).DeleteAsync()).Affected);
        Assert.Equal(2, (await shelf.DeleteAsync(all: true)).Affected);
        Assert.Equal(0, await shelf.CountAsync());
        await db.ExecAsync($"drop collection {name}");
        await db.ExecAsync($"drop collection {notes}");
    }
}
