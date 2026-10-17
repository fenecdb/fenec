using System.Net;
using System.Text;
using Xunit;

namespace FenecDb.Tests;

/// <summary>
/// <see cref="FenecClientOptions.ReadsNearest"/>: the builder's reads, and an <see cref="FenecClient.AsRead"/>
/// copy's requests, marked for a read replica near the client (<c>Fenec-Read: replica</c>) and sent after the
/// furthest position an answer from home named (<c>Fenec-After</c>, <c>Fenec-History</c>); writes, and every
/// request of a client reading home, sent as before; a read no copy holds that for (504) asked home with neither,
/// and home's position, which the 504 names, taken as the client's.
/// Against a scripted handler: what a router does with the mark is fenec-shard's to test, and the JavaScript
/// client's against real routers.
/// </summary>
public sealed class ReadsTests
{
    const string H1 = "1-00000000000000aa", H2 = "2-00000000000000bb";

    sealed class Script(params (int Status, string Body, string? Seq)[] answers) : HttpMessageHandler
    {
        readonly Queue<(int, string, string?)> _answers = new(answers);
        public readonly List<Dictionary<string, string>> Seen = [];

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            Seen.Add(request.Headers.ToDictionary(h => h.Key.ToLowerInvariant(), h => string.Join(",", h.Value)));
            var (status, body, seq) = _answers.Count > 0 ? _answers.Dequeue() : (200, "[]", null);
            var res = new HttpResponseMessage((HttpStatusCode)status)
            {
                Content = new StringContent(body, Encoding.UTF8, "application/json"),
            };
            if (seq is not null)
            {
                // Each of the history before the failover.
                res.Headers.TryAddWithoutValidation("Fenec-Seq", seq);
                res.Headers.TryAddWithoutValidation("Fenec-History", H1);
            }
            return Task.FromResult(res);
        }
    }

    [Fact]
    public async Task ReadsNearestAreMarkedAndSentAfterTheClientsWrites()
    {
        var script = new Script(
            (200, """{"affected":1}""", "9"),
            (200, """{"affected":1}""", "12"),
            (200, """{"affected":1}""", "10"),
            (200, """[{"n":1}]""", null),
            (200, """[{"count":2}]""", null),
            (200, """[{"count":2}]""", null),
            // A read near's answer moves nothing: it may be a replica's.
            (200, """{"ok":1,"results":[]}""", "15"),
            (504, $$"""{"error":"change 12 is not in this copy's history","seq":10,"history":"{{H2}}"}""", null),
            (200, """[{"n":1}]""", null),
            (200, """[{"n":1}]""", null));
        using var db = new FenecClient("http://script.invalid",
            new() { HttpClient = new HttpClient(script), ReadsNearest = true });
        foreach (var text in new[] { "put t {n: 1}", "put t {n: 2}", "put t {n: 3}" })
            await db.WithIdempotencyKey(text).ExecAsync(text);
        await db.From("t").RowsAsync();
        Assert.Equal(2, await db.From("t").CountAsync());
        await db.QueryAsync("get t count");
        await db.AsRead().BatchAsync([new Statement("get t count", [])]);
        Assert.Single(await db.From("t").RowsAsync());
        await db.From("t").RowsAsync();

        var seen = script.Seen;
        Assert.Equal(10, seen.Count);
        foreach (var w in seen.Take(3))
            Assert.False(w.ContainsKey("fenec-read") || w.ContainsKey("fenec-after"));
        // The furthest position, 12, not the last answered, 10.
        foreach (var i in new[] { 3, 4, 6, 7 })
        {
            Assert.Equal("replica", seen[i]["fenec-read"]);
            Assert.Equal("12", seen[i]["fenec-after"]);
            Assert.Equal(H1, seen[i]["fenec-history"]);
        }
        foreach (var i in new[] { 5, 8 })
            Assert.False(seen[i].ContainsKey("fenec-read") || seen[i].ContainsKey("fenec-after")
                || seen[i].ContainsKey("fenec-history"));
        // Home's position taken, lower and of the history after the failover.
        Assert.Equal("10", seen[9]["fenec-after"]);
        Assert.Equal(H2, seen[9]["fenec-history"]);
        Assert.Equal(10, db.Seq);
    }

    // Writes answered in turn from 100; the first marked read held until released, then 504 at 100; the third
    // 504 at an earlier term's 140.
    sealed class Racing : HttpMessageHandler
    {
        public readonly List<Dictionary<string, string>> Marked = [];
        public readonly TaskCompletionSource Arrived = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public readonly TaskCompletionSource Release = new(TaskCreationOptions.RunContinuationsAsynchronously);
        int _writes, _reads;

        static HttpResponseMessage Answer(int status, string body) =>
            new((HttpStatusCode)status) { Content = new StringContent(body, Encoding.UTF8, "application/json") };

        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var body = request.Content is null ? "" : await request.Content.ReadAsStringAsync(ct);
            var headers = request.Headers.ToDictionary(h => h.Key.ToLowerInvariant(), h => string.Join(",", h.Value));
            if (body.Contains("put"))
            {
                var res = Answer(200, """{"affected":1}""");
                res.Headers.TryAddWithoutValidation("Fenec-Seq", (99 + Interlocked.Increment(ref _writes)).ToString());
                res.Headers.TryAddWithoutValidation("Fenec-History", H2);
                return res;
            }
            if (headers.ContainsKey("fenec-read"))
            {
                int n;
                lock (Marked)
                {
                    Marked.Add(headers);
                    n = ++_reads;
                }
                if (n == 1)
                {
                    Arrived.SetResult();
                    await Release.Task;
                    return Answer(504, $$"""{"error":"x","seq":100,"history":"{{H2}}"}""");
                }
                if (n == 3) return Answer(504, $$"""{"error":"x","seq":140,"history":"{{H1}}"}""");
            }
            return Answer(200, "[]");
        }
    }

    /// <summary>A read near's 504 sets the position back only over the one the read was sent after -- a write
    /// answered meanwhile is further, and set back the next read near would miss it -- and never to an earlier
    /// term, which a fenced old primary names.</summary>
    [Fact]
    public async Task A504SetsThePositionBackOverItsOwnAloneNeverToAnEarlierTerm()
    {
        var racing = new Racing();
        using var db = new FenecClient("http://script.invalid",
            new() { HttpClient = new HttpClient(racing), ReadsNearest = true });
        await db.ExecAsync("put t {n: 1}");
        var read = db.From("t").RowsAsync();
        await racing.Arrived.Task;
        await db.ExecAsync("put t {n: 2}");
        racing.Release.SetResult();
        await read;
        for (var i = 0; i < 3; i++) await db.From("t").RowsAsync();
        Assert.Equal(4, racing.Marked.Count);
        foreach (var i in new[] { 1, 3 })
        {
            Assert.Equal("101", racing.Marked[i]["fenec-after"]);
            Assert.Equal(H2, racing.Marked[i]["fenec-history"]);
        }
    }

    // Each answer of its own history.
    sealed class Lineages(params (int Status, string Body, string? Seq, string? History)[] answers)
        : HttpMessageHandler
    {
        readonly Queue<(int, string, string?, string?)> _answers = new(answers);
        public readonly List<Dictionary<string, string>> Seen = [];

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            Seen.Add(request.Headers.ToDictionary(h => h.Key.ToLowerInvariant(), h => string.Join(",", h.Value)));
            var (status, body, seq, history) = _answers.Count > 0 ? _answers.Dequeue() : (200, "[]", null, null);
            var res = new HttpResponseMessage((HttpStatusCode)status)
            {
                Content = new StringContent(body, Encoding.UTF8, "application/json"),
            };
            if (seq is not null)
            {
                res.Headers.TryAddWithoutValidation("Fenec-Seq", seq);
                res.Headers.TryAddWithoutValidation("Fenec-History", history);
            }
            return Task.FromResult(res);
        }
    }

    /// <summary>A tenant made again under its name starts again at term 1, of another lineage -- the top half of
    /// its history's id: home's position is taken, from a 504 and from an answer, its term lower. Refused as an
    /// earlier term, every read near waited its whole wait for good. Within one lineage an earlier term is still
    /// refused.</summary>
    [Fact]
    public async Task ATenantMadeAgainHasItsPositionTakenItsTermLower()
    {
        const string old = "3-aaaaaaaa000000c3", remade = "1-bbbbbbbb00000001", third = "1-cccccccc00000001";
        const string second = "2-cccccccc00000002";
        var script = new Lineages(
            (200, """{"affected":1}""", "50", old),
            (504, $$"""{"error":"not in this copy","seq":2,"history":"{{remade}}"}""", null, null),
            (200, "[]", null, null),
            (200, "[]", null, null),
            (200, """{"affected":1}""", "1", third),
            (200, "[]", null, null),
            (200, """{"affected":1}""", "9", second),
            (200, """{"affected":1}""", "90", third),
            (200, "[]", null, null));
        using var db = new FenecClient("http://script.invalid",
            new() { HttpClient = new HttpClient(script), ReadsNearest = true });
        await db.ExecAsync("put t {n: 1}");
        await db.From("t").RowsAsync();
        await db.From("t").RowsAsync();
        Assert.Equal("2", script.Seen[3]["fenec-after"]);
        Assert.Equal(remade, script.Seen[3]["fenec-history"]);
        await db.ExecAsync("put t {n: 2}");
        await db.From("t").RowsAsync();
        Assert.Equal("1", script.Seen[5]["fenec-after"]);
        Assert.Equal(third, script.Seen[5]["fenec-history"]);
        await db.ExecAsync("put t {n: 3}");
        await db.ExecAsync("put t {n: 4}");
        await db.From("t").RowsAsync();
        Assert.Equal("9", script.Seen[8]["fenec-after"]);
        Assert.Equal(second, script.Seen[8]["fenec-history"]);
    }

    /// <summary>An <see cref="FenecClient.AsRead"/> copy's subscription near is marked and sent after the client's
    /// position, as its reads are, and the server holds it until a copy has that write. Its 504 is a read near's:
    /// home's position, which it names, is the client's from then, and the stream opens home with no mark. Taken
    /// as an error, every stream near after a write lost in a failover was refused.</summary>
    [Fact]
    public async Task ASubscriptionNearIsSentAfterTheClientsWritesAndGoesHomeOnA504()
    {
        var script = new Script(
            (200, """{"affected":1}""", "12"),
            (504, $$"""{"error":"change 12 is not in this copy's history","seq":10,"history":"{{H2}}"}""", null),
            (200, "event: seed\ndata: {\"seq\":10,\"rows\":[{\"n\":1}]}\n\n", null));
        using var db = new FenecClient("http://script.invalid",
            new() { HttpClient = new HttpClient(script), ReadsNearest = true });
        await db.ExecAsync("put t {n: 1}");
        Event? seed = null;
        await foreach (var ev in db.AsRead().SubscribeAsync("t"))
        {
            seed = ev;
            break;
        }
        Assert.NotNull(seed);
        Assert.Equal(("seed", 10L), (seed.Type, seed.Seq));
        Assert.Single(seed.Rows);
        var seen = script.Seen;
        Assert.Equal(3, seen.Count);
        Assert.Equal("replica", seen[1]["fenec-read"]);
        Assert.Equal("12", seen[1]["fenec-after"]);
        Assert.Equal(H1, seen[1]["fenec-history"]);
        Assert.False(seen[2].ContainsKey("fenec-read") || seen[2].ContainsKey("fenec-after")
            || seen[2].ContainsKey("fenec-history"));
        Assert.Equal(10, db.Seq);
    }

    [Fact]
    public async Task AClientReadsHomeUnlessItAsksForNearest()
    {
        var script = new Script();
        using var db = new FenecClient("http://script.invalid", new() { HttpClient = new HttpClient(script) });
        await db.From("t").RowsAsync();
        await db.AsRead().QueryAsync("get t count");
        Assert.All(script.Seen, h => Assert.False(h.ContainsKey("fenec-read")));
    }
}
