using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace FenecDb;

/// <summary>A difference no open applies -- one that would lose data or could mean two things -- and how to
/// resolve it, as the engine writes them.</summary>
public sealed record SchemaRefusal(string Kind, string Collection, string? Field, string Message, string Fix);

/// <summary>What the engine found comparing the server's schema with the one the code declares, and did: the
/// FenecQL that adds what is missing, the migrations it recorded, and what it refused.</summary>
public sealed record SchemaPlan(
    bool Applied, bool Ran, IReadOnlyList<int> Migrations, IReadOnlyList<string> Statements,
    IReadOnlyList<SchemaRefusal> Refusals);

/// <summary>A schema the server's differs from in what no open applies: <see cref="Refusals"/> says each
/// difference.</summary>
public sealed class FenecSchemaException(IReadOnlyList<SchemaRefusal> refusals)
    : FenecException(409, Describe(refusals))
{
    /// <summary>Each difference, and how to resolve it.</summary>
    public IReadOnlyList<SchemaRefusal> Refusals { get; } = refusals;

    static string Describe(IReadOnlyList<SchemaRefusal> refusals)
    {
        var b = new StringBuilder("the database's schema differs from the code's:");
        foreach (var r in refusals) b.Append("\n  - ").Append(r.Message).Append("\n    ").Append(r.Fix);
        return b.ToString();
    }
}

/// <summary>A migration that makes a field again as the code declares it -- its index, its collation -- its
/// values copied: what changes an index, which no statement changes in place.</summary>
public sealed record Rebuild(
    [property: JsonPropertyName("collection")] string Collection,
    [property: JsonPropertyName("field")] string Field);

public sealed partial class FenecClient
{
    /// <summary>
    /// Checks the server's schema against <paramref name="fenecql"/> -- <c>create collection</c> and
    /// <c>create index</c> statements, a <c>schema.fenecql</c> file -- in the engine. The server owns its
    /// schema: it is compared, and nothing applied, unless <paramref name="migrate"/> -- with the server's
    /// token -- runs the migrations it has not recorded, in order, and adds what only adds, all one block. A
    /// migration is FenecQL text or a <see cref="Rebuild"/>. A difference that would lose data or could mean
    /// two things throws a <see cref="FenecSchemaException"/>.
    /// </summary>
    public async Task<SchemaPlan> SchemaAsync(
        string fenecql, IReadOnlyList<object>? migrations = null, bool migrate = false,
        CancellationToken cancellationToken = default)
    {
        var steps = (migrations ?? []).Select(m => m is Rebuild r ? (object)new { rebuild = r } : m).ToList();
        var body = JsonSerializer.SerializeToUtf8Bytes(new { format = 1, fenecql, migrations = steps });
        byte[] raw;
        try
        {
            (raw, _) = await SendAsync(HttpMethod.Post, migrate ? "/_schema/apply" : "/_schema/plan?mode=follow",
                "application/json", body, TimeSpan.Zero, cancellationToken).ConfigureAwait(false);
        }
        catch (FenecException e) when (e.Status == 409 && e.Message.StartsWith('{'))
        {
            // Refusals, which the answer holds as a plan does.
            raw = Encoding.UTF8.GetBytes(e.Message);
        }
        var plan = JsonSerializer.Deserialize<SchemaPlan>(raw, ByName)!;
        if (plan.Refusals.Count > 0) throw new FenecSchemaException(plan.Refusals);
        return plan;
    }
}
