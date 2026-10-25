using FenecDb;

using var db = new FenecClient("http://127.0.0.1:8080");

await db.ExecAsync("put notes {title: $1, year: $2, kind: $3}", ["Sand and stars", 2023, "book"]);

var rows = await db.QueryAsync<Note>("get notes where year >= $1 order year desc", [2020]);
Console.WriteLine("from 2020: " + string.Join(", ", rows.Select(r => r.Title)));

var hits = await db.QueryAsync<Note>("get notes select title match title $1 fuzzy", ["oasys"]);
Console.WriteLine("search: " + string.Join(", ", hits.Select(h => h.Title)));

try
{
    await db.QueryAsync("get nowhere");
}
catch (FenecException e)
{
    Console.WriteLine($"refused: {e.Status} {e.Message}");
}

record Note(string Title, int Year);
