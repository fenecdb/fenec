using System.Text.Json;
using System.Text.Json.Serialization;
using Xunit;

namespace FenecDb.Tests;

[Collection("servers")]
public sealed class ClientTests(Servers servers)
{
    static int _names;

    // A collection name no other test uses.
    static string Fresh(string prefix) => $"{prefix}_{Environment.ProcessId}_{Interlocked.Increment(ref _names)}";

    FenecClient Root() => new(servers.Primary, new FenecClientOptions { Token = Servers.RootToken });

    // 7.038531e-26: the float whose shortest text a double reader rounds away.
    static readonly float Tie = BitConverter.Int32BitsToSingle(0x15ae43fd);

    sealed record Hit(string Title, [property: JsonPropertyName("_score")] float Score);

    [Fact]
    public async Task CreatePutGetAndNearWithExactScores()
    {
        using var db = Root();
        var name = Fresh("docs");
        await db.ExecAsync($"create collection {name} (title text, embed vector<3> @hnsw(dot))");
        var w = await db.ExecAsync($"put {name} {{title: $1, embed: $2}}", ["first", new[] { 1f, 2f, 3f }]);
        Assert.Equal(1, w.Affected);
        Assert.True(w.Seq > 0 && db.Seq >= w.Seq);
        await db.ExecAsync($"put {name} {{title: $1, embed: $2}}", ["second", new ReadOnlyMemory<float>([0.5f, 0.25f, 0f])]);

        var rows = await db.QueryAsync($"get {name} where id = $1", [1]);
        Assert.Single(rows);
        Assert.Equal("first", rows[0].GetProperty("title").GetString());
        Assert.Equal([1f, 2f, 3f], rows[0].GetProperty("embed").Deserialize<float[]>()!);

        var hits = await db.QueryAsync<Hit>($"get {name} select title near embed $1 limit 5", [new[] { 1f, 0f, 0f }]);
        Assert.Equal([new Hit("first", 1f), new Hit("second", 0.5f)], hits);

        // A write's answer through QueryAsync is one row holding it.
        var del = await db.QueryAsync($"del {name} where id = 2");
        Assert.Equal(1, del[0].GetProperty("affected").GetInt32());
    }

    sealed class Row
    {
        public long Id { get; set; }
        public string? T { get; set; }
        public long N { get; set; }
        public double F { get; set; }
        public bool B { get; set; }
        public string? Gone { get; set; }
        public List<string>? Tags { get; set; }
        public float[]? Embed { get; set; }
        public JsonElement Meta { get; set; }
    }

    [Fact]
    public async Task ParamsOfEveryType()
    {
        using var db = Root();
        var name = Fresh("params");
        await db.ExecAsync($"create collection {name} (t text, n int, f float, b bool, gone text, tags [text], embed vector<5>, meta json)");
        float[] vec = [Tie, 0.1f, float.MaxValue, -1.5e-7f, float.Epsilon];
        await db.ExecAsync($"put {name} {{t: $1, n: $2, f: $3, b: $4, gone: $5, tags: $6, embed: $7, meta: $8}}",
            ["çağ \"quoted\"", -(1L << 53), 2.5, true, null, new List<string> { "a", "b" }, vec,
             new Dictionary<string, object?> { ["lang"] = "tr", ["rank"] = 2 }]);

        var got = await db.QueryAsync<Row>(
            $"get {name} where t = $1 and n = $2 and f = $3 and b = $4 and gone is null and tags has $5 and n in [$6, $2]",
            ["çağ \"quoted\"", -(1L << 53), 2.5f, true, "b", 7]);
        var g = Assert.Single(got);
        Assert.Equal("çağ \"quoted\"", g.T);
        Assert.Equal(-(1L << 53), g.N);
        Assert.Equal(2.5, g.F);
        Assert.True(g.B);
        Assert.Null(g.Gone);
        Assert.Equal(["a", "b"], g.Tags!);
        Assert.Equal("tr", g.Meta.GetProperty("lang").GetString());
        Assert.Equal(2, g.Meta.GetProperty("rank").GetInt32());
        Assert.Equal(vec.Select(BitConverter.SingleToInt32Bits), g.Embed!.Select(BitConverter.SingleToInt32Bits));

        await Assert.ThrowsAsync<ArgumentException>(() => db.QueryAsync($"get {name} where f = $1", [float.NaN]));
    }

    [Fact]
    public void EveryFloatReadsBackThroughADouble()
    {
        void Check(float f)
        {
            var s = Params.F32(f);
            Assert.Equal(BitConverter.SingleToInt32Bits(f), BitConverter.SingleToInt32Bits((float)double.Parse(s, System.Globalization.CultureInfo.InvariantCulture)));
        }
        Check(Tie);
        Check(-Tie);
        Assert.Equal("0.1", Params.F32(0.1f));
        for (long bits = 0; bits <= uint.MaxValue; bits += 4099)
        {
            var f = BitConverter.Int32BitsToSingle((int)(uint)bits);
            if (float.IsFinite(f)) Check(f);
        }
    }

    [Fact]
    public async Task RefusalsAreTypedExceptions()
    {
        using var db = Root();
        var e = await Assert.ThrowsAsync<FenecException>(() => db.QueryAsync("get no_such_collection_here"));
        Assert.Equal((404, "not_found"), (e.Status, e.Code));
        Assert.Contains("no_such_collection_here", e.Message);
        e = await Assert.ThrowsAsync<FenecException>(() => db.QueryAsync("get x wher"));
        Assert.Equal((400, "bad_request"), (e.Status, e.Code));

        var name = Fresh("dup");
        await db.ExecAsync($"create collection {name} (t text)");
        await db.ExecAsync($"insert {name} {{id: 1, t: $1}}", ["a"]);
        e = await Assert.ThrowsAsync<FenecException>(() => db.ExecAsync($"insert {name} {{id: 1, t: $1}}", ["b"]));
        Assert.Equal((409, "conflict"), (e.Status, e.Code));

        using var wrong = new FenecClient(servers.Primary, new FenecClientOptions { Token = "wrong" });
        e = await Assert.ThrowsAsync<FenecException>(() => wrong.QueryAsync("collections"));
        Assert.Equal(401, e.Status);
    }

    [Fact]
    public async Task AWriteThatMissesItsCountIsRefusedAndPutBack()
    {
        using var db = Root();
        var name = Fresh("require");
        await db.ExecAsync($"create collection {name} (name text, balance int)");
        var accounts = db.From(name);
        await accounts.InsertAsync(new Dictionary<string, object?> { ["name"] = "a", ["balance"] = 10 });
        var e = await Assert.ThrowsAsync<FenecException>(() =>
            accounts.Where("name", "=", "nobody").UpdateAsync(new { balance = 0 }, require: 1));
        Assert.Equal((412, "unmet"), (e.Status, e.Code));
        Assert.Contains("requires 1", e.Message);
        var met = await accounts.Where("name", "=", "a").UpdateAsync(new { balance = 5 }, require: 1);
        Assert.Equal(1, met.Affected);

        // A batch whose second write is unmet keeps nothing of the first.
        var (debit, debitParams) = accounts.Where("name", "=", "a").ToUpdate(new { balance = 0 }, require: 1);
        var (gone, goneParams) = accounts.Where("name", "=", "nobody").ToDelete(require: 1);
        e = await Assert.ThrowsAsync<FenecException>(() => db.BatchAsync([new(debit, debitParams), new(gone, goneParams)]));
        Assert.Equal((412, "unmet", 0), (e.Status, e.Code, e.Completed));
        var rows = await db.QueryAsync($"get {name} select balance");
        Assert.Equal([5L], rows.Select(r => r.GetProperty("balance").GetInt64()));
    }

    [Fact]
    public async Task ABatchLandsWholeOrNotAtAll()
    {
        using var db = Root();
        var name = Fresh("batch");
        await db.ExecAsync($"create collection {name} (t text, n int)");
        var ok = await db.BatchAsync([
            new($"put {name} {{t: $1, n: $2}}", ["a", 1]),
            new($"put {name} {{t: $1, n: $2}}", ["b", 2]),
            new($"get {name} select t where n >= $1 order n", [1]),
        ]);
        Assert.Equal(3, ok.Ok);
        Assert.Equal(1, ok.Results[0].GetProperty("affected").GetInt32());
        Assert.Equal(2, ok.Results[2].GetProperty("rows").GetArrayLength());
        Assert.True(ok.Seq > 0);

        var e = await Assert.ThrowsAsync<FenecException>(() => db.BatchAsync([
            new($"put {name} {{t: $1, n: $2}}", ["c", 3]),
            new($"del {name} where n = 1"),
            new($"put {name} {{nofield: 1}}"),
        ]));
        Assert.Equal((404, 0), (e.Status, e.Completed));
        var rows = await db.QueryAsync($"get {name} select t order n");
        Assert.Equal(["a", "b"], rows.Select(r => r.GetProperty("t").GetString()));
    }

    [Fact]
    public async Task HighlightsAndFacetsComeBackInTheirShapes()
    {
        using var db = Root();
        var name = Fresh("marks");
        await db.ExecAsync($"create collection {name} (kind text @hash, body text @text)");
        await db.From(name).InsertAsync(new object[]
        {
            new { kind = "a", body = "rust is fast" },
            new { kind = "a", body = "rust and go" },
            new { kind = "b", body = "rust tools" },
            new { kind = "c", body = "python only" },
        });

        var q = db.From(name).Select("body").Highlight("body").Match("body", "rust").Facet("kind");
        var answer = await q.AnswerAsync();
        Assert.Equal(3, answer.Rows.Count);
        // Offsets into the text, UTF-16 code units: "rust" is the first four.
        Assert.All(answer.Rows, r => Assert.Equal("[[0,4]]", r.GetProperty("highlight(body)").GetRawText()));
        var kinds = Assert.Single(answer.Facets);
        Assert.Equal("kind", kinds.Key);
        Assert.Equal([("a", 2L), ("b", 1L)], kinds.Value.Select(c => (c.Value.GetString(), c.Count)));
        // The rows alone, from the same answer.
        Assert.Equal(3, (await q.RowsAsync()).Count);
        Assert.Equal(3, await db.From(name).Where("kind", "!=", "c").Facet("kind").CountAsync());

        var tagged = await db.From(name).Select("kind").Highlight("body", "<b>", "</b>").Snippet("body", 2)
            .Match("body", "tools").RowsAsync();
        var row = Assert.Single(tagged);
        Assert.Equal("rust <b>tools</b>", row.GetProperty("highlight(body)").GetString());
        var snippet = row.GetProperty("snippet(body)");
        Assert.Equal("rust tools", snippet.GetProperty("text").GetString());
        Assert.Equal("[[5,10]]", snippet.GetProperty("marks").GetRawText());

        // Without facets the answer is the bare array, and holds none.
        Assert.Empty((await db.AnswerAsync($"get {name} where kind = $1", ["c"])).Facets);
        var batch = await db.BatchAsync([new($"get {name} where kind = $1 facet kind", ["b"])]);
        var b = Answer.Of(batch.Results[0]);
        Assert.Single(b.Rows);
        Assert.Equal(1, b.Facets["kind"][0].Count);
    }

    [Fact]
    public async Task AnIdempotencyKeyIsReplayed()
    {
        using var db = Root();
        var name = Fresh("idem");
        await db.ExecAsync($"create collection {name} (t text)");
        var keyed = db.WithIdempotencyKey(name + "-1");
        var first = await keyed.ExecAsync($"put {name} {{t: $1}}", ["once"]);
        var again = await keyed.ExecAsync($"put {name} {{t: $1}}", ["once"]);
        Assert.False(first.Replayed);
        Assert.True(again.Replayed);
        Assert.Equal(1, again.Affected);
        var count = await db.QueryAsync($"get {name} count");
        Assert.Equal(1, count[0].GetProperty("count").GetInt32());
        var e = await Assert.ThrowsAsync<FenecException>(() => keyed.ExecAsync($"put {name} {{t: $1}}", ["another"]));
        Assert.Equal((422, "key_reused"), (e.Status, e.Code));
    }

    [Fact]
    public async Task AfterReadsYourWriteOnAReplica()
    {
        var replica = servers.Serve("--file", Path.Combine(servers.Scratch, "replica.fenec"),
            "--replication-token", Servers.ReplToken, "--replica-of", servers.Primary, "--sync", "50");
        using var db = Root();
        var name = Fresh("after");
        await db.ExecAsync($"create collection {name} (t text)");
        var w = await db.ExecAsync($"put {name} {{t: $1}}", ["written"]);
        using var r = new FenecClient(replica);
        var rows = await r.After(w.Seq).QueryAsync($"get {name} select t");
        Assert.Equal("written", Assert.Single(rows).GetProperty("t").GetString());
        var e = await Assert.ThrowsAsync<FenecException>(() => r.ExecAsync($"put {name} {{t: $1}}", ["refused"]));
        Assert.Equal(403, e.Status);
    }

    [Fact]
    public async Task ASubscriptionHearsAChange()
    {
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(20));
        using var db = Root();
        var name = Fresh("sub");
        await db.ExecAsync($"create collection {name} (t text, n int)");
        await db.ExecAsync($"put {name} {{t: $1, n: $2}}", ["old", 10]);
        var events = db.SubscribeAsync(name, [new("n", "gte.10")], cts.Token).GetAsyncEnumerator(cts.Token);
        try
        {
            Assert.True(await events.MoveNextAsync());
            var seed = events.Current;
            Assert.Equal("seed", seed.Type);
            Assert.Equal("old", Assert.Single(seed.Rows).GetProperty("t").GetString());

            await db.ExecAsync($"put {name} {{t: $1, n: $2}}", ["outside", 1]);
            await db.ExecAsync($"put {name} {{t: $1, n: $2}}", ["new", 11]);
            // The row outside the shape comes as the deletion of an id the
            // subscriber never held: an unscoped shape tells of every id that
            // changed and does not match.
            Event change;
            do
            {
                Assert.True(await events.MoveNextAsync());
                change = events.Current;
            } while (change.Type == "change" && change.Puts.Count == 0);
            Assert.Equal("change", change.Type);
            Assert.Equal("new", Assert.Single(change.Puts).GetProperty("t").GetString());
            Assert.True(change.Seq > seed.Seq);

            await db.ExecAsync($"del {name} where t = $1", ["old"]);
            Assert.True(await events.MoveNextAsync());
            Assert.Equal([1L], events.Current.Dels);
        }
        finally
        {
            await cts.CancelAsync();
            try { await events.DisposeAsync(); } catch (OperationCanceledException) { }
        }

        var e = await Assert.ThrowsAsync<FenecException>(async () =>
        {
            await foreach (var _ in db.SubscribeAsync(Fresh("missing"))) { }
        });
        Assert.Equal(404, e.Status);
    }

    [Fact]
    public async Task ChangesHandsOverEveryWrite()
    {
        using var db = Root();
        var name = Fresh("cdc");
        var before = await db.ExecAsync($"create collection {name} (t text)");
        await db.ExecAsync($"put {name} [{{t: $1}}, {{t: $2}}]", ["a", "b"]);
        var last = await db.ExecAsync($"del {name} where t = $1", ["a"]);

        var mine = new List<Change>();
        for (var since = before.Seq - 1; since < last.Seq;)
        {
            var got = await db.ChangesAsync(since, TimeSpan.FromSeconds(5));
            mine.AddRange(got.Writes.Where(c => c.Collection == name));
            since = got.Next;
        }
        Assert.Equal(["create:", "put:a", "put:b", "del:"],
            mine.Select(c => $"{c.Op}:{(c.Doc is { } d ? d.GetProperty("t").GetString() : "")}"));
        Assert.Equal((1L, last.Seq), (mine[3].Id!.Value, mine[3].Seq));

        // Nothing after the last write: the wait runs out with none.
        var clock = System.Diagnostics.Stopwatch.StartNew();
        var empty = await db.ChangesAsync(db.Seq, TimeSpan.FromMilliseconds(300));
        Assert.Empty(empty.Writes);
        Assert.Equal(db.Seq, empty.Next);
        Assert.True(clock.ElapsedMilliseconds >= 250);
        var e = await Assert.ThrowsAsync<FenecException>(() => db.ChangesAsync(db.Seq + 1000));
        Assert.Equal(409, e.Status);
    }

    [Fact]
    public async Task ATenantPath()
    {
        var dir = Directory.CreateDirectory(Path.Combine(servers.Scratch, "tenants")).FullName;
        var node = servers.Serve("--dir", dir, "--admin-token", "dotnet-admin");
        using (var http = new HttpClient())
        {
            using var req = new HttpRequestMessage(HttpMethod.Put, node + "/_admin/tenants/acme");
            req.Headers.Authorization = new("Bearer", "dotnet-admin");
            Assert.Equal(201, (int)(await http.SendAsync(req)).StatusCode);
        }
        using var acme = new FenecClient(node, new FenecClientOptions { Tenant = "acme" });
        await acme.ExecAsync("create collection notes (t text)");
        await acme.ExecAsync("put notes {t: $1}", ["acme's"]);
        Assert.Equal("acme's", Assert.Single(await acme.QueryAsync("get notes select t")).GetProperty("t").GetString());
        await acme.HealthAsync();
        using var nobody = new FenecClient(node, new FenecClientOptions { Tenant = "nobody" });
        var e = await Assert.ThrowsAsync<FenecException>(() => nobody.QueryAsync("collections"));
        Assert.Equal(404, e.Status);
    }

    [Fact]
    public async Task AScopedToken()
    {
        using var db = Root();
        try { await db.ExecAsync("create collection notes (title text, owner text)"); }
        catch (FenecException taken) when (taken.Status == 409) { } // made by a test before
        await db.ExecAsync("put notes {title: $1, owner: $2}", ["bob's", "bob"]);
        using var alice = new FenecClient(servers.Primary, new FenecClientOptions { Token = servers.Mint("{\"sub\":\"alice\"}") });
        await alice.ExecAsync("put notes {title: $1}", ["alice's"]);
        var rows = await alice.QueryAsync("get notes select title, owner");
        Assert.All(rows, r => Assert.Equal("alice", r.GetProperty("owner").GetString()));
        Assert.NotEmpty(rows);
        var e = await Assert.ThrowsAsync<FenecException>(() => alice.ExecAsync("put notes {title: $1, owner: $2}", ["forged", "bob"]));
        Assert.Equal((403, "forbidden"), (e.Status, e.Code));
    }

    [Fact]
    public async Task Health()
    {
        // No token needed.
        using var db = new FenecClient(servers.Primary);
        await db.HealthAsync();
    }

    [Fact]
    public async Task ASchemaIsComparedAndAppliedOnlyWhenAsked()
    {
        using var db = Root();
        var name = Fresh("schema");
        var v1 = $"create collection {name} (title text required, n int @hash)";
        await db.ExecAsync(v1);
        Assert.Empty((await db.SchemaAsync(v1)).Refusals);
        // A field the server lacks: the server's to add.
        var v2 = $"create collection {name} (title text required, n int @hash, at timestamp)";
        var e = await Assert.ThrowsAsync<FenecSchemaException>(() => db.SchemaAsync(v2));
        Assert.Equal("field_missing", e.Refusals[0].Kind);
        // Asked, it is added; a rename is a migration, run once.
        Assert.True((await db.SchemaAsync(v2, migrate: true)).Applied);
        var v3 = $"create collection {name} (name text required, n int @hash, at timestamp)";
        await Assert.ThrowsAsync<FenecSchemaException>(() => db.SchemaAsync(v3, migrate: true));
        object[] moved = [$"alter collection {name} rename field title to name"];
        Assert.Equal([1], (await db.SchemaAsync(v3, moved, migrate: true)).Migrations);
        Assert.Empty((await db.SchemaAsync(v3, moved, migrate: true)).Migrations);
        await db.ExecAsync("drop collection _migrations");
    }
}
