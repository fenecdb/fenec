// Npgsql and pgvector-dotnet against fenec-pg. Each check prints its name;
// the first that fails ends the run with a non-zero exit, as a test runner
// would, without one more dependency to restore.
using Npgsql;
using Pgvector;

var cs = Environment.GetEnvironmentVariable("FENEC_PG_NPGSQL")
    ?? throw new Exception("FENEC_PG_NPGSQL is not set");
var failed = 0;
void Check(string name, Action body)
{
    try { body(); Console.WriteLine($"ok   {name}"); }
    catch (Exception e) { failed++; Console.WriteLine($"FAIL {name}: {e.Message}"); }
}
void Expect(bool cond, string what) { if (!cond) throw new Exception(what); }

// Npgsql opens every connection with one text of several catalog queries,
// two of them starting with a comment: read from the comment, they were
// taken for FenecQL and the connection refused.
await using (var conn = new NpgsqlConnection(cs))
{
    Check("opens a connection", () => conn.Open());
    Check("creates a collection", () => new NpgsqlCommand(
        "create collection if not exists net_docs (title text, year int @hash, embed vector<3> @hnsw(cosine))",
        conn).ExecuteNonQuery());
    Check("writes with typed parameters", () =>
    {
        using var c = new NpgsqlCommand("put net_docs {title: $1, year: $2, embed: [0.1, 0.2, 0.3]}", conn);
        c.Parameters.AddWithValue("Night at the oasis");
        c.Parameters.AddWithValue(2024L);
        Expect(c.ExecuteNonQuery() == 1, "one row written");
    });
    Check("reads typed rows", () =>
    {
        using var c = new NpgsqlCommand("get net_docs select title, year where year >= $1", conn);
        c.Parameters.AddWithValue(2020L);
        using var r = c.ExecuteReader();
        Expect(r.Read(), "a row");
        Expect(r.GetString(0) == "Night at the oasis" && r.GetInt64(1) == 2024, "its values");
    });
    Check("commits a transaction", () =>
    {
        using var tx = conn.BeginTransaction();
        new NpgsqlCommand("put net_docs {title: 'in a transaction', year: 2025, embed: [0.3, 0.2, 0.1]}", conn, tx)
            .ExecuteNonQuery();
        tx.Commit();
    });
    Check("rolls a transaction back", () =>
    {
        using var tx = conn.BeginTransaction();
        new NpgsqlCommand("put net_docs {title: 'rolled back', year: 2026, embed: [0.2, 0.2, 0.2]}", conn, tx)
            .ExecuteNonQuery();
        tx.Rollback();
        var n = new NpgsqlCommand("get net_docs where title = 'rolled back' count", conn).ExecuteScalar();
        Expect(Convert.ToInt64(n) == 0, "the rolled back row is gone");
    });
}

// pgvector-dotnet registers the vector type by name and sends it binary.
var builder = new NpgsqlDataSourceBuilder(cs);
builder.UseVector();
await using var source = builder.Build();
await using (var conn = source.CreateConnection())
{
    Check("opens with UseVector", () => conn.Open());
    Check("writes a Vector parameter", () =>
    {
        using var c = new NpgsqlCommand("put net_docs {title: 'vector parameter', year: 2026, embed: $1}", conn);
        c.Parameters.AddWithValue(new Vector(new float[] { 0.1f, 0.25f, 0.3f }));
        Expect(c.ExecuteNonQuery() == 1, "one row written");
    });
    Check("searches near a Vector and reads Vectors back", () =>
    {
        using var c = new NpgsqlCommand("get net_docs select title, embed near embed $1 limit 3", conn);
        c.Parameters.AddWithValue(new Vector(new float[] { 0.1f, 0.2f, 0.3f }));
        using var r = c.ExecuteReader();
        Expect(r.Read(), "a nearest row");
        Expect(r.GetString(0) == "Night at the oasis", "the nearest is the identical vector");
        var v = r.GetFieldValue<Vector>(1).ToArray();
        Expect(v.Length == 3 && Math.Abs(v[1] - 0.2f) < 1e-6, "the vector as written");
        Expect(Convert.ToDouble(r["_score"]) > 0.99, "its score");
    });
}

Console.WriteLine(failed == 0 ? "all passed" : $"{failed} failed");
return failed == 0 ? 0 : 1;
