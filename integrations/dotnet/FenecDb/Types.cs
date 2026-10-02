using System.Text.Json;

namespace FenecDb;

/// <summary>What a write answers: how many documents it touched, a create's message, the change it left the
/// database at (<c>Fenec-Seq</c>, what <see cref="FenecClient.After"/> takes on a replica; 0 for an answer
/// replayed for its key), and whether the answer is the one kept for its idempotency key.</summary>
public sealed record ExecResult(long Affected, string? Message, long Seq, bool Replayed);

/// <summary>One statement of a batch.</summary>
public sealed record Statement(string Query, IReadOnlyList<object?>? Parameters = null);

/// <summary>What a batch answers: how many statements ran, each one's answer -- <c>{"affected": n}</c> or
/// <c>{"rows": [...]}</c> -- and the change the batch left the database at.</summary>
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
    internal static Event Failed(string error) => new("error", 0, [], [], [], false, error);

    internal static Event Of(string name, string data)
    {
        try
        {
            using var doc = JsonDocument.Parse(data);
            var d = doc.RootElement;
            if (name == "error")
                return Failed(d.TryGetProperty("error", out var e) ? e.GetString() ?? data : data);
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
/// holds: the since to read on from, so that nothing is missed or had twice.</summary>
public sealed record Changes(IReadOnlyList<Change> Writes, long Next);

/// <summary>A request the server refused: its HTTP status, the status's name and the server's message.</summary>
public class FenecException : Exception
{
    /// <summary>The HTTP status.</summary>
    public int Status { get; }

    /// <summary>The status's name: <c>bad_request</c> (400), <c>unauthorized</c> (401), <c>forbidden</c> (403),
    /// <c>not_found</c> (404), <c>conflict</c> (409), <c>gone</c> (410), <c>key_reused</c> (422),
    /// <c>unavailable</c> (503), <c>timeout</c> (504), <c>storage_full</c> (507), or <c>http_&lt;status&gt;</c>.</summary>
    public string Code { get; }

    /// <summary>How many statements of a failed batch were applied: 0, since a batch lands whole or not at all,
    /// but for one holding a compact, which runs each statement on its own and keeps what ran.</summary>
    public int Completed { get; }

    /// <summary>A refusal with its status, message and, for a batch, how many statements were applied.</summary>
    public FenecException(int status, string message, int completed = 0) : base(message)
    {
        Status = status;
        Code = CodeOf(status);
        Completed = completed;
    }

    static string CodeOf(int status) => status switch
    {
        400 => "bad_request",
        401 => "unauthorized",
        403 => "forbidden",
        404 => "not_found",
        409 => "conflict",
        410 => "gone",
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
                    r.TryGetProperty("completed", out var c) ? c.GetInt32() : 0);
        }
        catch (JsonException) { }
        return new FenecException(status, body.Trim());
    }
}
