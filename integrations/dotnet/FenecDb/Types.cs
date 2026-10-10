using System.Text.Json;

namespace FenecDb;

/// <summary>What a write answers: how many documents it touched, a create's message, the change it left the
/// database at (<c>Fenec-Seq</c>, what <see cref="FenecClient.After"/> takes on a replica; 0 for an answer
/// replayed for its key), and whether the answer is the one kept for its idempotency key. A write with
/// <c>returning</c> hands back its rows too -- an update's as written, a delete's as they were -- and
/// <c>Affected</c> is their number; null for any other write.</summary>
public sealed record ExecResult(long Affected, string? Message, long Seq, bool Replayed,
    IReadOnlyList<JsonElement>? Rows = null);

/// <summary>One value a <c>facet</c> counted and how many of the rows the query matched hold it. The value is any
/// JSON value a field holds -- null too, for the rows whose field is null.</summary>
public sealed record FacetCount(JsonElement Value, long Count);

/// <summary>
/// A query's rows, and what its <c>facet</c> clauses counted beside them: each field asked, in the order asked,
/// its values most first. The counts cover every row the query matched, not the page, which is why they come
/// beside the rows and never in one. Empty when the query asked none.
/// </summary>
public sealed record Answer(IReadOnlyList<JsonElement> Rows, IReadOnlyDictionary<string, IReadOnlyList<FacetCount>> Facets)
{
    static readonly IReadOnlyDictionary<string, IReadOnlyList<FacetCount>> None =
        new Dictionary<string, IReadOnlyList<FacetCount>>();

    /// <summary>An answer as the server writes it: a bare array of rows, <c>{"rows": [...], "facets": {...}}</c>
    /// when the query asked facets -- what <c>/query</c> answers, and each rows item of a batch's
    /// <see cref="BatchResult.Results"/>. Any other object, a write's <c>{"affected": n}</c>, is one row
    /// holding it.</summary>
    public static Answer Of(JsonElement body)
    {
        if (body.ValueKind == JsonValueKind.Array) return new(body.EnumerateArray().Select(e => e.Clone()).ToList(), None);
        if (IsAnswer(body))
            return new(body.GetProperty("rows").EnumerateArray().Select(e => e.Clone()).ToList(), FacetsOf(body));
        return new([body.Clone()], None);
    }

    // `{"rows": [...]}` with `facets` at most beside it: a write's answer or a create's message is an object too,
    // and stays the one row it was.
    static bool IsAnswer(JsonElement body) =>
        body.ValueKind == JsonValueKind.Object
        && body.EnumerateObject().All(p => p.Name is "rows" or "facets")
        && body.TryGetProperty("rows", out var v) && v.ValueKind == JsonValueKind.Array;

    static IReadOnlyDictionary<string, IReadOnlyList<FacetCount>> FacetsOf(JsonElement body)
    {
        if (!body.TryGetProperty("facets", out var f) || f.ValueKind != JsonValueKind.Object) return None;
        // A Dictionary nothing is removed from enumerates in the order it was filled: the order asked.
        var facets = new Dictionary<string, IReadOnlyList<FacetCount>>();
        foreach (var p in f.EnumerateObject())
            facets[p.Name] = p.Value.EnumerateArray()
                .Select(c => new FacetCount(c.GetProperty("value").Clone(), c.GetProperty("count").GetInt64()))
                .ToList();
        return facets;
    }
}

/// <summary>One statement of a batch.</summary>
public sealed record Statement(string Query, IReadOnlyList<object?>? Parameters = null);

/// <summary>What a batch answers: how many statements ran, each one's answer -- <c>{"affected": n}</c> or
/// <c>{"rows": [...]}</c>, with <c>"facets"</c> beside the rows when it asked them (<see cref="Answer.Of"/> reads
/// either) -- and the change the batch left the database at.</summary>
public sealed record BatchResult(int Ok, IReadOnlyList<JsonElement> Results, long Seq, bool Replayed);

/// <summary>
/// One event of a subscription. <see cref="Type"/> is <c>seed</c> -- <see cref="Rows"/> is the whole shape, which
/// replaces what the subscriber held -- <c>change</c> -- <see cref="Puts"/> and <see cref="Dels"/> since the last
/// event -- or <c>error</c>, the last event, with <see cref="Error"/>.
/// </summary>
public sealed record Event(
    string Type,
    long Seq,
    IReadOnlyList<JsonElement> Rows,
    IReadOnlyList<JsonElement> Puts,
    IReadOnlyList<long> Dels,
    bool Schema,
    string? Error)
{
    /// <summary>An <c>error</c> event's status, where the server gave one: 401 when it ended the stream at its
    /// token's <c>exp</c>, for the caller to subscribe again with a fresh token.</summary>
    public int? Status { get; init; }

    internal static Event Failed(string error) => new("error", 0, [], [], [], false, error);

    internal static Event Of(string name, string data)
    {
        try
        {
            using var doc = JsonDocument.Parse(data);
            var d = doc.RootElement;
            if (name == "error")
                return Failed(d.TryGetProperty("error", out var e) ? e.GetString() ?? data : data) with
                {
                    Status = d.TryGetProperty("status", out var st) && st.TryGetInt32(out var n) ? n : null,
                };
            return new Event(
                name,
                d.TryGetProperty("seq", out var s) ? s.GetInt64() : 0,
                List(d, "rows"),
                List(d, "puts"),
                d.TryGetProperty("dels", out var dels) ? dels.EnumerateArray().Select(x => x.GetInt64()).ToList() : [],
                d.TryGetProperty("schema", out var sc) && sc.GetBoolean(),
                null);
        }
        catch (JsonException e)
        {
            return Failed($"an event of the subscription: {e.Message}");
        }
    }

    static IReadOnlyList<JsonElement> List(JsonElement d, string name) =>
        d.TryGetProperty(name, out var v) ? v.EnumerateArray().Select(x => x.Clone()).ToList() : [];
}

/// <summary>One write on the server's disk: its number, when the primary wrote it (ms since the epoch), the
/// collection, <c>put</c>, <c>del</c>, <c>create</c>, <c>alter</c> or <c>drop</c>, the document's id, and a put's
/// document as written.</summary>
public sealed record Change(long Seq, long At, string Collection, string Op, long? Id, JsonElement? Doc);

/// <summary>What <see cref="FenecClient.ChangesAsync"/> hands over, and <see cref="Next"/> the last write it
/// holds: the since to read on from, so that nothing is missed or had twice. <see cref="Seq"/> is the last write the
/// database holds (<c>Fenec-Seq</c>): a <see cref="Next"/> that has reached it has every write there is.</summary>
public sealed record Changes(IReadOnlyList<Change> Writes, long Next, long Seq);

/// <summary>A request the server refused: its HTTP status, the status's name and the server's message.</summary>
public class FenecException : Exception
{
    /// <summary>The HTTP status.</summary>
    public int Status { get; }

    /// <summary>The status's name: <c>bad_request</c> (400), <c>unauthorized</c> (401), <c>forbidden</c> (403),
    /// <c>not_found</c> (404), <c>conflict</c> (409), <c>gone</c> (410), <c>unmet</c> (412: a write's require
    /// not met, its batch put back), <c>key_reused</c> (422),
    /// <c>unavailable</c> (503), <c>timeout</c> (504), <c>storage_full</c> (507), or <c>http_&lt;status&gt;</c>.</summary>
    public string Code { get; }

    /// <summary>How many statements of a failed batch were applied: 0, since a batch lands whole or not at all,
    /// but for one holding a compact, which runs each statement on its own and keeps what ran.</summary>
    public int Completed { get; }

    /// <summary>The statement of a failed batch that stopped it, from 0: the write whose require was not met,
    /// the put whose id was taken. <c>null</c> for anything but a batch's stop.</summary>
    public int? At { get; }

    /// <summary>A refusal with its status, message and, for a batch, how many statements were applied and
    /// which one stopped it.</summary>
    public FenecException(int status, string message, int completed = 0, int? at = null) : base(message)
    {
        Status = status;
        Code = CodeOf(status);
        Completed = completed;
        At = at;
    }

    static string CodeOf(int status) => status switch
    {
        400 => "bad_request",
        401 => "unauthorized",
        403 => "forbidden",
        404 => "not_found",
        409 => "conflict",
        410 => "gone",
        412 => "unmet",
        422 => "key_reused",
        503 => "unavailable",
        504 => "timeout",
        507 => "storage_full",
        _ => $"http_{status}",
    };

    internal static FenecException From(int status, string body)
    {
        try
        {
            using var doc = JsonDocument.Parse(body);
            var r = doc.RootElement;
            if (r.ValueKind == JsonValueKind.Object && r.TryGetProperty("error", out var e))
                return new FenecException(status, e.GetString() ?? body,
                    r.TryGetProperty("completed", out var c) ? c.GetInt32() : 0,
                    r.TryGetProperty("at", out var at) ? at.GetInt32() : null);
        }
        catch (JsonException) { }
        return new FenecException(status, body.Trim());
    }
}
