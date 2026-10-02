// Notes over fenec-server's HTTP endpoint, with the FenecDb SDK.
//
//   dotnet run                            seeds if empty, then lists
//   dotnet run -- add <title> <body> [tag ...]
//   dotnet run -- list [--tag T] [--open]
//   dotnet run -- search <words>
//   dotnet run -- done <id>
//   dotnet run -- watch                   the open notes again after every change
//   dotnet run -- smoke                   what CI runs
using System.Text;
using FenecDb;

var url = Environment.GetEnvironmentVariable("FENEC_URL") ?? "http://127.0.0.1:8080";
var token = Environment.GetEnvironmentVariable("FENEC_TOKEN") ?? "secret"; // a dev default; never ship one
using var db = new FenecClient(url, new() { Token = token });
// Makes what is missing; refuses what would lose data.
await db.SchemaAsync(File.ReadAllText(Path.Combine(AppContext.BaseDirectory, "schema.fenecql")), migrate: true);
var app = new Notes(db);

switch (args.FirstOrDefault())
{
    case "add":
        await app.AddAsync(args[1], args[2], args[3..]);
        break;
    case "list":
        var i = Array.IndexOf(args, "--tag");
        Notes.Show(await app.ListAsync(i >= 0 ? args[i + 1] : null, args.Contains("--open")));
        break;
    case "search":
        var (match, fused) = await app.SearchAsync(string.Join(' ', args[1..]));
        Console.WriteLine("match: " + string.Join(", ", match.Select(n => n.Title)));
        Console.WriteLine("fuse:  " + string.Join(", ", fused.Select(n => n.Title)));
        break;
    case "done":
        await db.From("notes").Where("id", "=", long.Parse(args[1])).UpdateAsync(new { done = true });
        break;
    case "watch":
        // A subscription to the open notes over server-sent events: each event, the list again.
        await foreach (var ev in db.SubscribeAsync("notes", [new("done", "eq.false")]))
        {
            if (ev.Error is not null) throw new Exception(ev.Error);
            Console.WriteLine("--");
            Notes.Show(await app.ListAsync(open: true));
        }
        break;
    case "smoke":
        await app.SmokeAsync();
        break;
    default:
        await app.SeedAsync();
        Notes.Show(await app.ListAsync());
        break;
}

record Note(long Id, string Title, string[]? Tags, bool Done, string? At);

class Notes(FenecClient db)
{
    static readonly (string Title, string Body, string[] Tags, bool Done, string At)[] Seeds =
    [
        ("Groceries", "Buy milk, eggs and fresh bread for the weekend.", ["home", "shopping"], false, "2026-09-28T09:00:00Z"),
        ("Release checklist", "Tag the release, publish the packages and update the docs.", ["work"], false, "2026-09-29T09:00:00Z"),
        ("Book flights", "Find cheap flights to Istanbul for the conference in spring.", ["travel", "work"], true, "2026-09-30T09:00:00Z"),
        ("Book club", "Finish the novel about the desert fox before Thursday.", ["home", "reading"], false, "2026-10-01T09:00:00Z"),
    ];

    /// A TOY embedding, a placeholder for a real model: hashed character trigrams (FNV-1a over the
    /// UTF-8 bytes) into 64 dimensions. It matches spelling, not meaning. A real one is an embeddings
    /// API call or a local ONNX model, with the field's dimension changed to match.
    public static float[] Embed(string text)
    {
        var lower = new string(text.Select(c => c is >= 'A' and <= 'Z' ? (char)(c + 32) : c).ToArray());
        var b = Encoding.UTF8.GetBytes(" " + lower + " ");
        var v = new float[64];
        for (var i = 0; i + 3 <= b.Length; i++)
        {
            var h = 0x811c9dc5u;
            for (var j = i; j < i + 3; j++) h = unchecked((h ^ b[j]) * 0x01000193u);
            v[h % 64] += 1;
        }
        var n = Math.Sqrt(v.Sum(x => (double)x * x));
        return n > 0 ? v.Select(x => (float)(x / n)).ToArray() : v;
    }

    public Task AddAsync(string title, string body, string[] tags, bool done = false, string? at = null) =>
        db.From("notes").InsertAsync(new
        {
            title, body, tags, done,
            at = at ?? DateTime.UtcNow.ToString("o"),
            embed = Embed($"{title} {body}"),
        });

    public async Task SeedAsync()
    {
        if (await db.From("notes").CountAsync() > 0) return;
        foreach (var s in Seeds) await AddAsync(s.Title, s.Body, s.Tags, s.Done, s.At);
    }

    public Task<IReadOnlyList<Note>> ListAsync(string? tag = null, bool open = false)
    {
        var q = db.From("notes").Select("id", "title", "tags", "done", "at").Order("at", "desc").Limit(20);
        if (tag is not null) q = q.Where("tags", "has", tag);
        if (open) q = q.Where("done", "=", false);
        return q.RowsAsync<Note>();
    }

    public async Task<(IReadOnlyList<Note> Match, IReadOnlyList<Note> Fused)> SearchAsync(string words)
    {
        var notes = db.From("notes").Select("id", "title");
        return (await notes.Match("body", words).Limit(5).RowsAsync<Note>(),
                await notes.Match("body", words).Near("embed", Embed(words)).Fuse().Limit(5).RowsAsync<Note>());
    }

    public static void Show(IEnumerable<Note> notes)
    {
        foreach (var n in notes)
            Console.WriteLine($"[{(n.Done ? "x" : " ")}] {n.Id,3}  {n.Title,-20} {string.Join(", ", n.Tags ?? [])}");
    }

    public async Task SmokeAsync()
    {
        static void Check(string step, bool ok)
        {
            Console.WriteLine((ok ? "ok   " : "FAIL ") + step);
            if (!ok) Environment.Exit(1);
        }
        await SeedAsync();
        Check("seeded 4 notes", await db.From("notes").CountAsync() == 4);
        var hello = Embed("hello");
        Check("toy embedding", Enumerable.Range(0, 64).Where(i => hello[i] != 0).SequenceEqual([24, 36, 46, 48, 62]));
        Check("newest first", (await ListAsync())[0].Title == "Book club");
        Check("by tag", (await ListAsync("work")).Select(n => n.Title).SequenceEqual(["Book flights", "Release checklist"]));
        Check("open", (await ListAsync(open: true)).Count == 3);
        Check("match", (await SearchAsync("release docs")).Match[0].Title == "Release checklist");
        var near = await db.From("notes").Near("embed", Embed("flights to Istanbul")).FirstAsync<Note>();
        Check("near", near?.Title == "Book flights");
        Check("fuse", (await SearchAsync("desert fox")).Fused[0].Title == "Book club");

        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(5));
        var seen = false;
        try
        {
            await foreach (var ev in db.SubscribeAsync("notes", [new("done", "eq.false")], timeout.Token))
            {
                if (ev.Type == "seed") await AddAsync("Call mom", "Ask about the weekend.", ["home"]);
                if (ev.Type == "change" && ev.Puts.Any(p => p.GetProperty("title").GetString() == "Call mom"))
                {
                    seen = true;
                    break;
                }
            }
        }
        catch (OperationCanceledException) { }
        Check("live subscription", seen);

        await db.From("notes").Where("title", "=", "Groceries").UpdateAsync(new { done = true });
        Check("done", (await ListAsync(open: true)).Count == 3);
    }
}
