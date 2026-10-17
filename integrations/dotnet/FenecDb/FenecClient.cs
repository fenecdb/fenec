using System.Net.Http.Headers;
using System.Runtime.CompilerServices;
using System.Text;
using System.Text.Json;

namespace FenecDb;

/// <summary>How a <see cref="FenecClient"/> reaches its server.</summary>
public sealed class FenecClientOptions
{
    /// <summary>Sent as <c>Authorization: Bearer</c>: the server's --http-token, or a JSON Web Token held to its policy.</summary>
    public string? Token { get; init; }

    /// <summary>Sends every request under <c>/t/&lt;tenant&gt;/</c>: a tenant of a node started with --dir, or of a fenec-shard router.</summary>
    public string? Tenant { get; init; }

    /// <summary>Bounds each request, 30 seconds unless given; <see cref="Timeout.InfiniteTimeSpan"/> is none.
    /// A subscription is bounded by its token alone, and <see cref="FenecClient.ChangesAsync"/> waits its wait on top.</summary>
    public TimeSpan Timeout { get; init; } = TimeSpan.FromSeconds(30);

    /// <summary>Sends the requests through this client rather than one of its own, which is then not disposed.
    /// Its own Timeout, if it has one, also cuts a subscription short.</summary>
    public HttpClient? HttpClient { get; init; }

    /// <summary>Marks the builder's reads, and an <see cref="FenecClient.AsRead"/> copy's requests, for a read
    /// replica near the client (<c>Fenec-Read: replica</c>): a fenec-shard router near a region holding the
    /// tenant's read replica answers them there, the rest go to the tenant's home. Each is sent after the furthest
    /// position an answer from home named to this client -- the change and the history it is of
    /// (<c>Fenec-After</c>, <c>Fenec-History</c>) -- so it reads nothing older than it wrote, nor a write a
    /// failover replaced; one no copy holds that for -- the write lost in a failover of home -- is asked again at
    /// home without it, and home's position, which its 504 names, is the client's from then.</summary>
    public bool ReadsNearest { get; init; }
}

/// <summary>
/// fenec-server's HTTP endpoint. A statement is one <c>POST /query</c>, several are one
/// <c>POST /batch</c> under one write lock; values go in as <c>$1</c>, <c>$2</c>, ... and never
/// into the text. Safe to use from several threads at once; <see cref="After"/>,
/// <see cref="WithIdempotencyKey"/> and <see cref="WithWait"/> hand out copies that share its connections.
/// </summary>
public sealed partial class FenecClient : IDisposable
{
    internal static readonly JsonSerializerOptions ByName = new() { PropertyNameCaseInsensitive = true };

    readonly HttpClient _http;
    readonly bool _owns;
    readonly string _root;
    readonly string _base;
    readonly string? _token;
    readonly TimeSpan _timeout;
    readonly Position _seq;
    readonly long _after;
    readonly string? _key;
    readonly TimeSpan _wait;
    // Reads go near (ReadsNearest); this copy's requests only read (AsRead, the builder's reads).
    readonly bool _near;
    readonly bool _read;

    // Where an answer from home left the database: its change (Fenec-Seq), the history that change is of
    // (Fenec-History, <term>-<id>; none for a database that never had a replica) and that history's term, one
    // more at each promotion. Shared by the copies.
    sealed class Position
    {
        readonly object _lock = new();
        long _value;
        long _term;
        string? _history;

        public long Value { get { lock (_lock) return _value; } }

        public (long, string?) Get() { lock (_lock) return (_value, _history); }

        // The furthest seen -- of another lineage, home's word on a tenant made again under the name, whose terms
        // start again; or a later term of the same, or further in one: answers arriving out of order on several
        // threads must not move it back.
        public void Note(long n, string? history)
        {
            var term = TermOf(history);
            lock (_lock)
                if (Another(history, _history) || term > _term || (term == _term && n > _value))
                    (_value, _term, _history) = (n, term, history);
        }

        // Home's, lower or not, which a read near's 504 names when no copy held the one it was sent after
        // (sent, sentHistory) -- a write lost in a failover: kept, every read near would wait it out for good.
        // Taken only over the position sent -- a write answered meanwhile is further, and set back the next read
        // near would miss it -- and never of an earlier term of the same lineage: a router whose copy of the
        // directory lags a failover sends the read to the old primary, fenced, which names the branch it
        // abandoned. Another lineage's is a tenant made again under the name, which starts again at term 1.
        public void Told(long sent, string? sentHistory, long n, string? history)
        {
            var term = TermOf(history);
            lock (_lock)
                if (_value == sent && _history == sentHistory
                    && (term >= TermOf(sentHistory) || Another(history, sentHistory)))
                    (_value, _term, _history) = (n, term, history);
        }

        static long TermOf(string? history) =>
            history is not null && long.TryParse(history.Split('-')[0], out var t) ? t : 0;

        // A history's lineage: its id's top half, which every fork of the tenant keeps and a tenant made again
        // under its name does not.
        static string? LineageOf(string? history)
        {
            var dash = history?.IndexOf('-') ?? -1;
            return dash >= 0 && history!.Length >= dash + 9 ? history.Substring(dash + 1, 8) : null;
        }

        // Whether history a is of another lineage than b.
        static bool Another(string? a, string? b)
        {
            var (la, lb) = (LineageOf(a), LineageOf(b));
            return la is not null && lb is not null && la != lb;
        }
    }

    /// <summary>A client for the server at <paramref name="url"/>, such as <c>http://127.0.0.1:8080</c>.</summary>
    public FenecClient(string url, FenecClientOptions? options = null)
    {
        options ??= new FenecClientOptions();
        _root = url.TrimEnd('/');
        _base = options.Tenant is null ? _root : $"{_root}/t/{Uri.EscapeDataString(options.Tenant)}";
        _token = options.Token;
        _timeout = options.Timeout;
        _owns = options.HttpClient is null;
        // Its own client has no timeout of its own: a subscription lasts as
        // long as it is read, and each request is bounded through its token.
        _http = options.HttpClient ?? new HttpClient { Timeout = System.Threading.Timeout.InfiniteTimeSpan };
        _seq = new Position();
        _near = options.ReadsNearest;
    }

    FenecClient(FenecClient from, long after, string? key, TimeSpan wait, bool read)
    {
        (_http, _owns, _root, _base, _token, _timeout, _seq, _near) =
            (from._http, false, from._root, from._base, from._token, from._timeout, from._seq, from._near);
        (_after, _key, _wait, _read) = (after, key, wait, read);
    }

    /// <summary>The furthest change an answer from home -- a write's, a read-only batch's -- left the database at,
    /// through this client or a copy of it; 0 before any.</summary>
    public long Seq => _seq.Value;

    /// <summary>A copy whose requests the server answers only once it holds change <paramref name="seq"/> --
    /// a write's <see cref="ExecResult.Seq"/> on the primary, read on a replica -- or with a 504 after five seconds.
    /// Never from before the write.</summary>
    public FenecClient After(long seq) => new(this, seq, _key, _wait, _read);

    /// <summary>A copy whose requests only read: on a client with <see cref="FenecClientOptions.ReadsNearest"/> a
    /// router near the tenant's read replica answers them there. The builder's reads are sent so on their own; a
    /// statement the caller knows reads goes through this -- one that writes is refused (403), never landed.</summary>
    public FenecClient AsRead() => new(this, _after, _key, _wait, true);

    /// <summary>A copy whose writes carry the key: sent again after a timeout, a write is answered as the first
    /// time and not made twice (<see cref="ExecResult.Replayed"/>). One key per write; the same key with another
    /// request is a 422.</summary>
    public FenecClient WithIdempotencyKey(string key) => new(this, _after, key, _wait, _read);

    /// <summary>A copy whose <c>set</c> and <c>del</c> statements -- <see cref="ExecAsync"/>, a batch's, the
    /// builder's <c>UpdateAsync</c> and <c>DeleteAsync</c> -- the server holds while they write nothing, until
    /// they can or for <paramref name="wait"/>, at most 30 seconds (<c>Fenec-Wait</c>): a worker's claim waits
    /// for a job there -- one enqueued, a delayed one come due, a lease lapsed -- rather than sleeping and
    /// sending it again. The cancellation token gives it up as ever; the client's Timeout is <paramref name="wait"/>
    /// longer for it.</summary>
    public FenecClient WithWait(TimeSpan wait)
    {
        if (wait < TimeSpan.Zero) throw new ArgumentOutOfRangeException(nameof(wait), "a wait is not negative");
        return new(this, _after, _key, wait, _read);
    }

    /// <summary>The query builder over a collection: chain <c>Select</c>, <c>Where</c>, <c>Near</c>, <c>Order</c>,
    /// <c>Limit</c> ... and end with <c>RowsAsync</c>, <c>FirstAsync</c>, <c>CountAsync</c>, or a write --
    /// <c>InsertAsync</c>, <c>UpdateAsync</c>, <c>DeleteAsync</c>.</summary>
    public Query From(string collection) => new(collection, this);

    /// <summary>Runs one FenecQL statement and hands back its rows. A statement whose answer is not rows --
    /// a write's <c>{"affected": n}</c>, a create's <c>{"message": ...}</c> -- comes back as one row holding it.</summary>
    public async Task<IReadOnlyList<JsonElement>> QueryAsync(
        string query, IReadOnlyList<object?>? parameters = null, CancellationToken cancellationToken = default)
    {
        var (body, _) = await RunAsync(query, parameters, cancellationToken).ConfigureAwait(false);
        return RowsOf(body);
    }

    /// <summary>A builder's read: this client's own request, marked as one where it reads near.</summary>
    internal Task<(byte[], Head)> RunReadAsync(string query, IReadOnlyList<object?>? parameters, CancellationToken ct) =>
        (_near && !_read ? AsRead() : this).RunAsync(query, parameters, ct);

    /// <summary>Runs one FenecQL statement and maps its rows to <typeparamref name="T"/>, a record or a class,
    /// by property name, whatever its case. A <c>float[]</c> property reads a vector back as the f32s the server holds.</summary>
    public async Task<IReadOnlyList<T>> QueryAsync<T>(
        string query, IReadOnlyList<object?>? parameters = null, CancellationToken cancellationToken = default)
    {
        var (body, _) = await RunAsync(query, parameters, cancellationToken).ConfigureAwait(false);
        return RowsOf(body).Select(r => r.Deserialize<T>(ByName)!).ToList();
    }

    /// <summary>Runs one FenecQL statement and hands back its rows and what its <c>facet</c> clauses counted
    /// beside them -- <c>get products match title $1 limit 20 facet brand top 10</c>.</summary>
    public async Task<Answer> AnswerAsync(
        string query, IReadOnlyList<object?>? parameters = null, CancellationToken cancellationToken = default)
    {
        var (body, _) = await RunAsync(query, parameters, cancellationToken).ConfigureAwait(false);
        return AnswerOf(body);
    }

    /// <summary>Runs one FenecQL statement that writes -- put, insert, set, del, create, drop, alter -- and
    /// hands back what it did.</summary>
    public async Task<ExecResult> ExecAsync(
        string query, IReadOnlyList<object?>? parameters = null, CancellationToken cancellationToken = default)
    {
        var (body, head) = await RunAsync(query, parameters, cancellationToken).ConfigureAwait(false);
        using var doc = JsonDocument.Parse(body);
        // A write with returning answers the rows it wrote.
        if (doc.RootElement.ValueKind == JsonValueKind.Array)
        {
            var rows = AnswerOf(body).Rows;
            return new ExecResult(rows.Count, null, head.Seq, head.Replayed, rows);
        }
        if (doc.RootElement.ValueKind != JsonValueKind.Object)
            throw new InvalidOperationException("the statement answered rows, not a write: use QueryAsync");
        var r = doc.RootElement;
        return new ExecResult(
            r.TryGetProperty("affected", out var a) ? a.GetInt64() : 0,
            r.TryGetProperty("message", out var m) ? m.GetString() : null,
            head.Seq, head.Replayed);
    }

    /// <summary>Runs the statements in order under one write lock, as one block: their writes all land, or at the
    /// first error none of them do -- a <see cref="FenecException"/> whose <see cref="FenecException.Completed"/>
    /// says how many were applied. A batch holding a compact runs each statement on its own, and what ran before
    /// an error stays.</summary>
    public async Task<BatchResult> BatchAsync(
        IEnumerable<Statement> statements, CancellationToken cancellationToken = default)
    {
        using var buf = new MemoryStream();
        var first = true;
        foreach (var s in statements)
        {
            if (!first) buf.WriteByte((byte)'\n');
            first = false;
            Params.WriteBody(buf, s.Query, s.Parameters);
        }
        var (body, head) = await SendAsync(HttpMethod.Post, "/batch", "application/x-ndjson", buf.ToArray(),
            TimeSpan.Zero, cancellationToken).ConfigureAwait(false);
        using var doc = JsonDocument.Parse(body);
        var r = doc.RootElement;
        var results = r.TryGetProperty("results", out var rs)
            ? rs.EnumerateArray().Select(e => e.Clone()).ToList()
            : new List<JsonElement>();
        return new BatchResult(r.GetProperty("ok").GetInt32(), results, head.Seq, head.Replayed);
    }

    /// <summary>
    /// Streams a collection's rows that match <paramref name="filter"/>, and every change to them, over
    /// server-sent events (<c>GET /&lt;collection&gt;/changes</c>): a <c>seed</c> first, then each <c>change</c>
    /// as it lands. The filter is the REST surface's query string -- <c>("year", "gte.2024")</c>,
    /// <c>("where", "&lt;a FenecQL condition&gt;")</c>, <c>("select", "title,year")</c>, <c>("since", "&lt;seq&gt;")</c>
    /// to resume -- and takes no parameters. It ends when the token is cancelled; a stream that ends for any
    /// other reason yields an <c>error</c> event last.
    /// </summary>
    public async IAsyncEnumerable<Event> SubscribeAsync(
        string collection, IEnumerable<KeyValuePair<string, string>>? filter = null,
        [EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        var url = $"{_base}/{Uri.EscapeDataString(collection)}/changes";
        var q = string.Join("&", (filter ?? []).Select(kv =>
            $"{Uri.EscapeDataString(kv.Key)}={Uri.EscapeDataString(kv.Value)}"));
        if (q.Length > 0) url += "?" + q;
        using var req = Request(HttpMethod.Get, url);
        req.Headers.Accept.ParseAdd("text/event-stream");
        using var res = await _http.SendAsync(req, HttpCompletionOption.ResponseHeadersRead, cancellationToken)
            .ConfigureAwait(false);
        if (!res.IsSuccessStatusCode)
        {
            var raw = await res.Content.ReadAsByteArrayAsync(cancellationToken).ConfigureAwait(false);
            // A stream near waits for the client's position as a read does, and a 504 is a read near's: no copy
            // holds it, home either -- the write lost in a failover of home, which says where it stands, the
            // client's position from here on -- and the stream opens home with no mark. Taken as an error, every
            // stream near after it was refused for good.
            if (!SentHome(req, (int)res.StatusCode, raw))
                throw FenecException.From((int)res.StatusCode, Encoding.UTF8.GetString(raw));
            await foreach (var ev in new FenecClient(this, 0, _key, _wait, false)
                               .SubscribeAsync(collection, filter, cancellationToken).ConfigureAwait(false))
                yield return ev;
            yield break;
        }
        using var reader = new StreamReader(
            await res.Content.ReadAsStreamAsync(cancellationToken).ConfigureAwait(false), Encoding.UTF8);
        string? name = null;
        var data = new StringBuilder();
        while (true)
        {
            string? line;
            try
            {
                line = await reader.ReadLineAsync(cancellationToken).ConfigureAwait(false);
            }
            catch (Exception e) when (e is IOException or HttpRequestException && !cancellationToken.IsCancellationRequested)
            {
                line = null;
            }
            if (line is null)
            {
                cancellationToken.ThrowIfCancellationRequested();
                yield return Event.Failed("the subscription ended");
                yield break;
            }
            if (line.Length == 0)
            {
                if (name is not null || data.Length > 0)
                {
                    var ev = Event.Of(name ?? "message", data.ToString());
                    yield return ev;
                    if (ev.Type == "error") yield break;
                }
                name = null;
                data.Clear();
            }
            else if (line[0] == ':') { } // a keep-alive
            else if (line.StartsWith("event:", StringComparison.Ordinal)) name = line[6..].Trim();
            else if (line.StartsWith("data:", StringComparison.Ordinal))
                data.Append(line.AsSpan(line.Length > 5 && line[5] == ' ' ? 6 : 5));
        }
    }

    /// <summary>
    /// The writes on the server's disk after <paramref name="since"/> (<c>GET /_changes</c>; fenec-server
    /// --cdc, or a primary with --replication-token), 1 000 at most. With <paramref name="wait"/>, an answer
    /// that would hold none waits that long for a write, 30 seconds at most. A since the server no longer
    /// keeps is a 410 whose message says the first it does; one past the last write a 409.
    /// </summary>
    public async Task<Changes> ChangesAsync(
        long since, TimeSpan? wait = null, CancellationToken cancellationToken = default)
    {
        var path = $"/_changes?since={since}";
        if (wait is { } w && w > TimeSpan.Zero) path += $"&wait={(long)w.TotalMilliseconds}";
        var (body, head) = await SendAsync(HttpMethod.Get, path, null, null, wait ?? TimeSpan.Zero,
            cancellationToken).ConfigureAwait(false);
        var writes = new List<Change>();
        foreach (var line in Encoding.UTF8.GetString(body).Split('\n'))
        {
            if (string.IsNullOrWhiteSpace(line)) continue;
            using var doc = JsonDocument.Parse(line);
            var c = doc.RootElement;
            writes.Add(new Change(
                c.GetProperty("seq").GetInt64(),
                c.TryGetProperty("at", out var at) ? at.GetInt64() : 0,
                c.GetProperty("collection").GetString()!,
                c.GetProperty("op").GetString()!,
                c.TryGetProperty("id", out var id) ? id.GetInt64() : null,
                c.TryGetProperty("doc", out var d) ? d.Clone() : null));
        }
        var next = head.Next ?? since;
        return new Changes(writes, next, Math.Max(head.Seq, next));
    }

    /// <summary>Asks <c>GET /_health</c>, which takes no token and no lock; throws when the server does not
    /// answer. A tenant's is its node's.</summary>
    public async Task HealthAsync(CancellationToken cancellationToken = default)
    {
        using var cts = Bounded(TimeSpan.Zero, cancellationToken);
        using var res = await _http.GetAsync(_root + "/_health", cts.Token).ConfigureAwait(false);
        if (!res.IsSuccessStatusCode)
            throw FenecException.From((int)res.StatusCode,
                await res.Content.ReadAsStringAsync(cts.Token).ConfigureAwait(false));
    }

    /// <summary>Disposes the HttpClient the client made; one handed in is left to its owner.</summary>
    public void Dispose()
    {
        if (_owns) _http.Dispose();
    }

    internal readonly record struct Head(long Seq, bool Replayed, long? Next);

    internal Task<(byte[], Head)> RunAsync(string query, IReadOnlyList<object?>? parameters, CancellationToken ct)
    {
        using var buf = new MemoryStream();
        Params.WriteBody(buf, query, parameters);
        return SendAsync(HttpMethod.Post, "/query", "application/json", buf.ToArray(), TimeSpan.Zero, ct);
    }

    CancellationTokenSource Bounded(TimeSpan extra, CancellationToken ct)
    {
        var cts = CancellationTokenSource.CreateLinkedTokenSource(ct);
        if (_timeout != System.Threading.Timeout.InfiniteTimeSpan && _timeout > TimeSpan.Zero)
            cts.CancelAfter(_timeout + extra + _wait);
        return cts;
    }

    HttpRequestMessage Request(HttpMethod method, string url)
    {
        var req = new HttpRequestMessage(method, url);
        if (_token is not null) req.Headers.Authorization = new AuthenticationHeaderValue("Bearer", _token);
        var near = _read && _near;
        if (near) req.Headers.TryAddWithoutValidation("Fenec-Read", "replica");
        if (_after > 0)
            req.Headers.TryAddWithoutValidation("Fenec-After", _after.ToString());
        else if (near && _seq.Get() is ( > 0 and var at, var history))
        {
            req.Headers.TryAddWithoutValidation("Fenec-After", at.ToString());
            if (history is not null) req.Headers.TryAddWithoutValidation("Fenec-History", history);
        }
        if (_key is not null) req.Headers.TryAddWithoutValidation("Idempotency-Key", _key);
        if (_wait > TimeSpan.Zero)
            req.Headers.TryAddWithoutValidation("Fenec-Wait", ((long)_wait.TotalMilliseconds).ToString());
        return req;
    }

    // One request, its answer read whole: the body of a 2xx, a FenecException otherwise. The body goes as
    // bytes with its length: the server takes no chunked body (411).
    async Task<(byte[], Head)> SendAsync(HttpMethod method, string path, string? contentType, byte[]? body,
        TimeSpan extra, CancellationToken ct)
    {
        using var cts = Bounded(extra, ct);
        using var req = Request(method, _base + path);
        if (body is not null)
        {
            req.Content = new ByteArrayContent(body);
            req.Content.Headers.ContentType = new MediaTypeHeaderValue(contentType!);
        }
        using var res = await _http.SendAsync(req, cts.Token).ConfigureAwait(false);
        var raw = await res.Content.ReadAsByteArrayAsync(cts.Token).ConfigureAwait(false);
        // Asked home with no mark after a read near's 504, it is answered with what home holds.
        var near = _read && _near;
        if (SentHome(req, (int)res.StatusCode, raw))
            return await new FenecClient(this, 0, _key, _wait, false)
                .SendAsync(method, path, contentType, body, extra, ct).ConfigureAwait(false);
        if (!res.IsSuccessStatusCode)
            throw FenecException.From((int)res.StatusCode, Encoding.UTF8.GetString(raw));
        long seq = 0;
        // A read near's answer may be a replica's: it moves nothing.
        if (Header(res, "Fenec-Seq") is { } s && long.TryParse(s, out var n))
        {
            seq = n;
            if (!near) _seq.Note(n, Header(res, "Fenec-History"));
        }
        long? next = Header(res, "Fenec-Next") is { } x && long.TryParse(x, out var m) ? m : null;
        return (raw, new Head(seq, Header(res, "Idempotent-Replayed") == "true", next));
    }

    // Whether an answer is a 504 to a request near sent after the client's position: no copy the router reached
    // holds it, home either -- the write lost in a failover of home, which says where it stands, taken here as the
    // client's position from then on (one the caller named with After is not the client's to take). The request
    // goes home with no mark.
    bool SentHome(HttpRequestMessage req, int status, byte[] raw)
    {
        if (status != 504 || !(_read && _near) || !req.Headers.Contains("Fenec-After")) return false;
        if (_after == 0) Told(req, raw);
        return true;
    }

    // Home's position, from a read near's 504: its seq and history, over the position `req` was sent after.
    void Told(HttpRequestMessage req, byte[] raw)
    {
        try
        {
            using var doc = JsonDocument.Parse(raw);
            var r = doc.RootElement;
            if (r.ValueKind != JsonValueKind.Object || !r.TryGetProperty("seq", out var s)
                || !s.TryGetInt64(out var n))
                return;
            var history = r.TryGetProperty("history", out var h) && h.ValueKind == JsonValueKind.String
                ? h.GetString() : null;
            long.TryParse(Sent(req, "Fenec-After"), out var sent);
            _seq.Told(sent, Sent(req, "Fenec-History"), n, history);
        }
        catch (JsonException)
        {
        }
    }

    static string? Sent(HttpRequestMessage req, string name) =>
        req.Headers.TryGetValues(name, out var v) ? v.FirstOrDefault() : null;

    static string? Header(HttpResponseMessage res, string name) =>
        res.Headers.TryGetValues(name, out var v) ? v.FirstOrDefault() : null;

    // The rows of a bare array, or of `{"rows": [...], "facets": {...}}` when the query asked facets.
    internal static IReadOnlyList<JsonElement> RowsOf(byte[] body) => AnswerOf(body).Rows;

    internal static Answer AnswerOf(byte[] body)
    {
        using var doc = JsonDocument.Parse(body);
        return Answer.Of(doc.RootElement);
    }
}
