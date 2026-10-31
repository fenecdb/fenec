// Written by site/builder_tabs.py (make docs-builder): do not edit.

// The code of each builder tab on the site, as the page shows it, run
// against a transport that records and held to the statement its case of
// integrations/builder-golden.json makes: a tab shows code this SDK runs.
// make docs-builder writes the file again as the pages' tabs change, and
// site/builder_tabs.py --check fails while it is behind them.

using System.Net;
using System.Text;
using System.Text.Json;
using Xunit;

namespace FenecDb.Tests;

public sealed class DocsBuilderTests
{
    /// <summary>A server that answers as fenec-server would and keeps the last body it was sent.</summary>
    sealed class Recorder : HttpMessageHandler
    {
        public string? Body;

        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            Body = await request.Content!.ReadAsStringAsync(ct);
            var query = JsonDocument.Parse(Body).RootElement.GetProperty("query").GetString()!;
            var rows = query.EndsWith(" count") || query.Contains(" count lookup ") ? """[{"count":0}]""" : "[]";
            var answer = query.Split(' ')[0] is "put" or "set" or "del" ? """{"affected":0}"""
                : query.Contains(" facet ") ? """{"rows":""" + rows + ""","facets":{}}""" : rows;
            return new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent(answer, Encoding.UTF8, "application/json") };
        }
    }

    /// <summary>What the tabs call db: a client of a server that records.</summary>
    static FenecClient Db(out Recorder sent)
    {
        sent = new Recorder();
        return new FenecClient("http://docs.invalid", new() { HttpClient = new HttpClient(sent) });
    }

    static void Sends(Recorder sent, string text, string parameters)
    {
        var body = JsonDocument.Parse(sent.Body!).RootElement;
        Assert.Equal(text, body.GetProperty("query").GetString());
        var got = body.GetProperty("params");
        Assert.True(Same(JsonDocument.Parse(parameters).RootElement, got), $"params {got.GetRawText()}, want {parameters}");
    }

    // JSON compared by value: a number written 1 and 1.0 is one number to the server.
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

    [Fact(DisplayName = "docs: aggregates over every order")]
    public async Task AggregatesOverEveryOrder()
    {
        using var db = Db(out var sent);
        await db.From("orders")
            .Select("count(*)", "sum(total)", "avg(total)", "min(at)", "max(at)")
            .RowsAsync();
        Sends(sent, "get orders select count(*), sum(total), avg(total), min(at), max(at)", "[]");
    }

    [Fact(DisplayName = "docs: aggregates of each status")]
    public async Task AggregatesOfEachStatus()
    {
        using var db = Db(out var sent);
        await db.From("orders")
            .Select("status", "count(*)", "avg(total)")
            .Where("year", 2026)
            .Group("status")
            .RowsAsync();
        Sends(sent, "get orders select status, count(*), avg(total) where year = $1 group status", "[2026]");
    }

    [Fact(DisplayName = "docs: the ten customers who spent most")]
    public async Task TheTenCustomersWhoSpentMost()
    {
        using var db = Db(out var sent);
        await db.From("orders")
            .Select("customer", "sum(total)")
            .Group("customer")
            .Order("sum(total)", "desc")
            .Limit(10)
            .RowsAsync();
        Sends(sent, "get orders select customer, sum(total) group customer order sum(total) desc limit $1", "[10]");
    }

    [Fact(DisplayName = "docs: distinct users an hour and event")]
    public async Task DistinctUsersAnHourAndEvent()
    {
        using var db = Db(out var sent);
        await db.From("events")
            .Select(
                Computed.Bucket("at", "1h").As("hour"),
                "name",
                Computed.CountDistinct("user").As("users")
            )
            .Group("hour", "name")
            .RowsAsync();
        Sends(sent, "get events select bucket(at, 1h) as hour, name, count(distinct user) as users group hour, name", "[]");
    }

    [Fact(DisplayName = "docs: one-minute bars of a symbol")]
    public async Task OneMinuteBarsOfASymbol()
    {
        using var db = Db(out var sent);
        await db.From("ticks")
            .Select(
                Computed.Bucket("at", "1m").As("bar"),
                Computed.First("px", "at").As("open"),
                Computed.Expr("max(px)").As("high"),
                Computed.Expr("min(px)").As("low"),
                Computed.Last("px", "at").As("close"),
                Computed.Expr("sum(qty)").As("volume")
            )
            .Where("sym", "S07")
            .Group("bar")
            .RowsAsync();
        Sends(sent, "get ticks select bucket(at, 1m) as bar, first(px by at) as open, max(px) as high, min(px) as low, last(px by at) as close, sum(qty) as volume where sym = $1 group bar", "[\"S07\"]");
    }

    [Fact(DisplayName = "docs: vwap of each symbol")]
    public async Task VwapOfEachSymbol()
    {
        using var db = Db(out var sent);
        await db.From("ticks")
            .Select("sym", Computed.Expr("sum(px * qty) / sum(qty)").As("vwap"))
            .Group("sym")
            .RowsAsync();
        Sends(sent, "get ticks select sym, sum(px * qty) / sum(qty) as vwap group sym", "[]");
    }

    [Fact(DisplayName = "docs: totals of a json path")]
    public async Task TotalsOfAJsonPath()
    {
        using var db = Db(out var sent);
        await db.From("events")
            .Select("props.plan", "sum(props.v)")
            .Group("props.plan")
            .RowsAsync();
        Sends(sent, "get events select props.plan, sum(props.v) group props.plan", "[]");
    }

    [Fact(DisplayName = "docs: the three statuses that took most")]
    public async Task TheThreeStatusesThatTookMost()
    {
        using var db = Db(out var sent);
        await db.From("orders")
            .Select("status", "count(*)", "sum(total)")
            .Where("year", 2026)
            .Group("status")
            .Order("sum(total)", "desc")
            .Limit(3)
            .RowsAsync();
        Sends(sent, "get orders select status, count(*), sum(total) where year = $1 group status order sum(total) desc limit $2", "[2026, 3]");
    }

    [Fact(DisplayName = "docs: one-minute bars since a time")]
    public async Task OneMinuteBarsSinceATime()
    {
        using var db = Db(out var sent);
        await db.From("ticks")
            .Select(
                Computed.Bucket("at", "1m").As("bar"),
                Computed.First("px", "at").As("open"),
                Computed.Expr("max(px)").As("high"),
                Computed.Expr("min(px)").As("low"),
                Computed.Last("px", "at").As("close"),
                Computed.Expr("sum(qty)").As("volume")
            )
            .Where("sym", "S07")
            .Where("at", ">=", "2026-05-03T09:00:00Z")
            .Group("bar")
            .RowsAsync();
        Sends(sent, "get ticks select bucket(at, 1m) as bar, first(px by at) as open, max(px) as high, min(px) as low, last(px by at) as close, sum(qty) as volume where sym = $1 and at >= $2 group bar", "[\"S07\", \"2026-05-03T09:00:00Z\"]");
    }

    [Fact(DisplayName = "docs: the newest articles")]
    public async Task TheNewestArticles()
    {
        using var db = Db(out var sent);
        await db.From("articles")
            .Order("published", "desc")
            .Limit(20)
            .RowsAsync();
        Sends(sent, "get articles order published desc limit $1", "[20]");
    }

    [Fact(DisplayName = "docs: the newest articles since a day")]
    public async Task TheNewestArticlesSinceADay()
    {
        using var db = Db(out var sent);
        await db.From("articles")
            .Where("published", ">=", "2026-09-01")
            .Order("published", "desc")
            .Limit(20)
            .RowsAsync();
        Sends(sent, "get articles where published >= $1 order published desc limit $2", "[\"2026-09-01\", 20]");
    }

    [Fact(DisplayName = "docs: articles counted over a range of titles")]
    public async Task ArticlesCountedOverARangeOfTitles()
    {
        using var db = Db(out var sent);
        await db.From("articles")
            .Where("title", ">=", "k")
            .Where("title", "<", "l")
            .CountAsync();
        Sends(sent, "get articles where title >= $1 and title < $2 count", "[\"k\", \"l\"]");
    }

    [Fact(DisplayName = "docs: the nearest articles since a day")]
    public async Task TheNearestArticlesSinceADay()
    {
        using var db = Db(out var sent);
        await db.From("articles")
            .Where("published", ">=", "2026-01-01")
            .Near("embed", new[] { 0.9, 0.1, 0 })
            .Limit(10)
            .RowsAsync();
        Sends(sent, "get articles where published >= $1 near embed $2 limit $3", "[\"2026-01-01\", [0.9, 0.1, 0], 10]");
    }

    [Fact(DisplayName = "docs: an owner's newest posts")]
    public async Task AnOwnerSNewestPosts()
    {
        using var db = Db(out var sent);
        await db.From("posts")
            .Where("owner", 7)
            .Order("created", "desc")
            .Limit(20)
            .RowsAsync();
        Sends(sent, "get posts where owner = $1 order created desc limit $2", "[7, 20]");
    }

    [Fact(DisplayName = "docs: three owners' newest posts")]
    public async Task ThreeOwnersNewestPosts()
    {
        using var db = Db(out var sent);
        await db.From("posts")
            .Where("owner", "in", new object?[] { 7, 8, 9 })
            .Order("created", "desc")
            .Limit(20)
            .RowsAsync();
        Sends(sent, "get posts where owner in [$1, $2, $3] order created desc limit $4", "[7, 8, 9, 20]");
    }

    [Fact(DisplayName = "docs: an owner's posts counted since a time")]
    public async Task AnOwnerSPostsCountedSinceATime()
    {
        using var db = Db(out var sent);
        await db.From("posts")
            .Where("owner", 7)
            .Where("created", ">=", "2026-10-01")
            .CountAsync();
        Sends(sent, "get posts where owner = $1 and created >= $2 count", "[7, \"2026-10-01\"]");
    }

    [Fact(DisplayName = "docs: a session seen")]
    public async Task ASessionSeen()
    {
        using var db = Db(out var sent);
        await db.From("sessions")
            .InsertAsync(new { user = "ada", seen = Computed.Expr("now()") });
        Sends(sent, "put sessions {user: $1, seen: now()}", "[\"ada\"]");
    }

    [Fact(DisplayName = "docs: a session seen again")]
    public async Task ASessionSeenAgain()
    {
        using var db = Db(out var sent);
        await db.From("sessions")
            .Where("user", "ada")
            .UpdateAsync(new { seen = Computed.Expr("now()") });
        Sends(sent, "set sessions {seen: now()} where user = $1", "[\"ada\"]");
    }

    [Fact(DisplayName = "docs: a user's sessions")]
    public async Task AUserSSessions()
    {
        using var db = Db(out var sent);
        await db.From("sessions").Where("user", "ada").RowsAsync();
        Sends(sent, "get sessions where user = $1", "[\"ada\"]");
    }

    [Fact(DisplayName = "docs: products with their newest reviews")]
    public async Task ProductsWithTheirNewestReviews()
    {
        using var db = Db(out var sent);
        await db.From("products")
            .Limit(20)
            .Lookup(
                "reviews",
                on: "product_id",
                order: [new SortKey("created", "desc")],
                limit: 3
            )
            .RowsAsync();
        Sends(sent, "get products limit $1 lookup reviews on product_id order created desc limit $2", "[20, 3]");
    }

    [Fact(DisplayName = "docs: products with their reviews")]
    public async Task ProductsWithTheirReviews()
    {
        using var db = Db(out var sent);
        await db.From("products")
            .Lookup("reviews", on: "product_id")
            .RowsAsync();
        Sends(sent, "get products lookup reviews on product_id", "[]");
    }

    [Fact(DisplayName = "docs: the names of dear products with their good reviews")]
    public async Task TheNamesOfDearProductsWithTheirGoodReviews()
    {
        using var db = Db(out var sent);
        await db.From("products")
            .Select("name")
            .Where("price", ">=", 10000)
            .Lookup(
                "reviews",
                on: "product_id",
                select: ["body"],
                where: Cond.Cmp("stars", "gte", 4),
                limit: 3
            )
            .RowsAsync();
        Sends(sent, "get products select name where price >= $1 lookup reviews on product_id select body where stars >= $2 limit $3", "[10000, 4, 3]");
    }

    [Fact(DisplayName = "docs: products with their tags by sku")]
    public async Task ProductsWithTheirTagsBySku()
    {
        using var db = Db(out var sent);
        await db.From("products")
            .Lookup("tags", on: "sku", parentKey: "sku")
            .RowsAsync();
        Sends(sent, "get products lookup tags on sku = sku", "[]");
    }

    [Fact(DisplayName = "docs: shops, their orders and the orders' lines")]
    public async Task ShopsTheirOrdersAndTheOrdersLines()
    {
        using var db = Db(out var sent);
        await db.From("shops")
            .Limit(20)
            .Lookup("orders", on: "shop_id", limit: 3)
            .Lookup("lines", on: "order_id", limit: 5)
            .RowsAsync();
        Sends(sent, "get shops limit $1 lookup orders on shop_id limit $2 lookup lines on order_id limit $3", "[20, 3, 5]");
    }

    [Fact(DisplayName = "docs: orders with their lines by a key of two fields")]
    public async Task OrdersWithTheirLinesByAKeyOfTwoFields()
    {
        using var db = Db(out var sent);
        await db.From("orders")
            .Where("org", 7)
            .Limit(20)
            .Lookup(
                "lines",
                on: new[] { "org", "order" },
                parentKey: new[] { "org", "id" },
                order: [new SortKey("line")],
                limit: 5
            )
            .RowsAsync();
        Sends(sent, "get orders where org = $1 limit $2 lookup lines on (org, order) = (org, id) order line asc limit $3", "[7, 20, 5]");
    }

    [Fact(DisplayName = "docs: shops with their orders folded")]
    public async Task ShopsWithTheirOrdersFolded()
    {
        using var db = Db(out var sent);
        await db.From("shops")
            .Limit(20)
            .Lookup(
                "orders",
                on: "shop",
                select: [Computed.Expr("count(*)").As("n"), Computed.Expr("sum(total)").As("revenue")]
            )
            .RowsAsync();
        Sends(sent, "get shops limit $1 lookup orders on shop select count(*) as n, sum(total) as revenue", "[20]");
    }

    [Fact(DisplayName = "docs: couriers with their latest fifty deliveries folded")]
    public async Task CouriersWithTheirLatestFiftyDeliveriesFolded()
    {
        using var db = Db(out var sent);
        await db.From("couriers")
            .Limit(20)
            .Lookup(
                "deliveries",
                on: "courier",
                select: [Computed.Expr("count(*)").As("n"), Computed.Expr("avg(km)").As("km")],
                order: [new SortKey("at", "desc")],
                limit: 50
            )
            .RowsAsync();
        Sends(sent, "get couriers limit $1 lookup deliveries on courier select count(*) as n, avg(km) as km order at desc limit $2", "[20, 50]");
    }

    [Fact(DisplayName = "docs: each shop's three best sellers")]
    public async Task EachShopSThreeBestSellers()
    {
        using var db = Db(out var sent);
        await db.From("shops")
            .Lookup(
                "lines",
                on: "shop",
                select: ["product", Computed.Expr("sum(qty)").As("sold")],
                group: ["product"],
                order: [new SortKey("sold", "desc")],
                limit: 3
            )
            .RowsAsync();
        Sends(sent, "get shops lookup lines on shop select product, sum(qty) as sold group product order sold desc limit $1", "[3]");
    }

    [Fact(DisplayName = "docs: the busiest couriers with their names")]
    public async Task TheBusiestCouriersWithTheirNames()
    {
        using var db = Db(out var sent);
        await db.From("orders")
            .Select("courier", Computed.Expr("count(*)").As("n"))
            .Where("at", ">=", "2026-10-01")
            .Group("courier")
            .Order("n", "desc")
            .Limit(10)
            .Lookup("couriers", on: "user", parentKey: "courier", select: ["name"])
            .RowsAsync();
        Sends(sent, "get orders select courier, count(*) as n where at >= $1 group courier order n desc limit $2 lookup couriers on user = courier select name", "[\"2026-10-01\", 10]");
    }

    [Fact(DisplayName = "docs: products with a five-star review")]
    public async Task ProductsWithAFiveStarReview()
    {
        using var db = Db(out var sent);
        await db.From("products")
            .Limit(20)
            .Lookup(
                "reviews",
                on: "product_id",
                required: true,
                where: Cond.Cmp("stars", "=", 5)
            )
            .RowsAsync();
        Sends(sent, "get products limit $1 lookup reviews on product_id required where stars = $2", "[20, 5]");
    }

    [Fact(DisplayName = "docs: products with a five-star review counted")]
    public async Task ProductsWithAFiveStarReviewCounted()
    {
        using var db = Db(out var sent);
        await db.From("products")
            .Lookup(
                "reviews",
                on: "product_id",
                required: true,
                where: Cond.Cmp("stars", "=", 5)
            )
            .CountAsync();
        Sends(sent, "get products count lookup reviews on product_id required where stars = $1", "[5]");
    }

    [Fact(DisplayName = "docs: articles a page at a time")]
    public async Task ArticlesAPageAtATime()
    {
        using var db = Db(out var sent);
        await db.From("articles")
            .Where("year", 2024)
            .Order("published", "desc")
            .Limit(20)
            .Offset(40)
            .RowsAsync();
        Sends(sent, "get articles where year = $1 order published desc limit $2 offset $3", "[2024, 20, 40]");
    }

    [Fact(DisplayName = "docs: a page of matches with its brands counted")]
    public async Task APageOfMatchesWithItsBrandsCounted()
    {
        using var db = Db(out var sent);
        await db.From("products")
            .Match("title", "tent")
            .Limit(20)
            .Facet("brand", top: 10)
            .RowsAsync();
        Sends(sent, "get products match title $1 limit $2 facet brand top $3", "[\"tent\", 20, 10]");
    }

    [Fact(DisplayName = "docs: ready jobs claimed a page at a time")]
    public async Task ReadyJobsClaimedAPageAtATime()
    {
        using var db = Db(out var sent);
        await db.From("jobs")
            .Where(Cond.Raw("run_at <= now()"))
            .Order("run_at")
            .Limit(10)
            .UpdateAsync(new { owner = "w1" }, returning: ["*"]);
        Sends(sent, "set jobs {owner: $1} where run_at <= now() order run_at asc limit $2 returning *", "[\"w1\", 10]");
    }

    [Fact(DisplayName = "docs: a column worked out")]
    public async Task AColumnWorkedOut()
    {
        using var db = Db(out var sent);
        await db.From("ticks")
            .Select("sym", Computed.Expr("px * qty").As("notional"))
            .Where("qty", ">", 100)
            .RowsAsync();
        Sends(sent, "get ticks select sym, px * qty as notional where qty > $1", "[100]");
    }

    [Fact(DisplayName = "docs: columns named and worked out")]
    public async Task ColumnsNamedAndWorkedOut()
    {
        using var db = Db(out var sent);
        await db.From("ticks")
            .Select(
                Computed.Expr("px").As("price"),
                Computed.Bucket("at", "1m").As("minute"),
                Computed.Expr("greatest(px, ?)", 100).As("floor")
            )
            .RowsAsync();
        Sends(sent, "get ticks select px as price, bucket(at, 1m) as minute, greatest(px, $1) as floor", "[100]");
    }

    [Fact(DisplayName = "docs: a column of a case")]
    public async Task AColumnOfACase()
    {
        using var db = Db(out var sent);
        await db.From("orders")
            .Select(
                "id",
                Computed.Expr("case when total >= ? then ? else ? end", 100, "large", "small").As("size")
            )
            .RowsAsync();
        Sends(sent, "get orders select id, case when total >= $1 then $2 else $3 end as size", "[100, \"large\", \"small\"]");
    }

    [Fact(DisplayName = "docs: articles counted since a year")]
    public async Task ArticlesCountedSinceAYear()
    {
        using var db = Db(out var sent);
        await db.From("articles").Where("year", ">=", 2024).CountAsync();
        Sends(sent, "get articles where year >= $1 count", "[2024]");
    }

    [Fact(DisplayName = "docs: a document with a json field")]
    public async Task ADocumentWithAJsonField()
    {
        using var db = Db(out var sent);
        await db.From("docs")
            .InsertAsync(new { title = "a", meta = new { lang = "tr", source = new { site = "x", rank = 3 } } });
        Sends(sent, "put docs {title: $1, meta: $2}", "[\"a\", {\"lang\": \"tr\", \"source\": {\"site\": \"x\", \"rank\": 3}}]");
    }

    [Fact(DisplayName = "docs: documents by paths")]
    public async Task DocumentsByPaths()
    {
        using var db = Db(out var sent);
        await db.From("docs")
            .Where("meta.lang", "tr")
            .Where("meta.source.rank", ">=", 2)
            .RowsAsync();
        Sends(sent, "get docs where meta.lang = $1 and meta.source.rank >= $2", "[\"tr\", 2]");
    }

    [Fact(DisplayName = "docs: a path selected and ordered by")]
    public async Task APathSelectedAndOrderedBy()
    {
        using var db = Db(out var sent);
        await db.From("docs")
            .Select("title", "meta.source.site")
            .Order("meta.source.rank", "desc")
            .RowsAsync();
        Sends(sent, "get docs select title, meta.source.site order meta.source.rank desc", "[]");
    }

    [Fact(DisplayName = "docs: a path set")]
    public async Task APathSet()
    {
        using var db = Db(out var sent);
        await db.From("docs")
            .Where("title", "a")
            .UpdateAsync(new Dictionary<string, object?> { ["meta.lang"] = "en" });
        Sends(sent, "set docs {meta.lang: $1} where title = $2", "[\"en\", \"a\"]");
    }

    [Fact(DisplayName = "docs: an event at a time written out")]
    public async Task AnEventAtATimeWrittenOut()
    {
        using var db = Db(out var sent);
        await db.From("events")
            .InsertAsync(new { name = "login", t = "2026-09-19T12:34:56Z" });
        Sends(sent, "put events {name: $1, t: $2}", "[\"login\", \"2026-09-19T12:34:56Z\"]");
    }

    [Fact(DisplayName = "docs: an event at epoch milliseconds")]
    public async Task AnEventAtEpochMilliseconds()
    {
        using var db = Db(out var sent);
        await db.From("events")
            .InsertAsync(new { name = "logout", t = 1758285296000 });
        Sends(sent, "put events {name: $1, t: $2}", "[\"logout\", 1758285296000]");
    }

    [Fact(DisplayName = "docs: events between a day and now")]
    public async Task EventsBetweenADayAndNow()
    {
        using var db = Db(out var sent);
        await db.From("events")
            .Where("t", ">=", "2026-01-01")
            .Where(Cond.Raw("t < now()"))
            .RowsAsync();
        Sends(sent, "get events where t >= $1 and t < now()", "[\"2026-01-01\"]");
    }

    [Fact(DisplayName = "docs: people in the root collation")]
    public async Task PeopleInTheRootCollation()
    {
        using var db = Db(out var sent);
        await db.From("people")
            .Order("name", "asc", collate: "und")
            .Limit(20)
            .RowsAsync();
        Sends(sent, "get people order name collate und asc limit $1", "[20]");
    }

    [Fact(DisplayName = "docs: people in Turkish order")]
    public async Task PeopleInTurkishOrder()
    {
        using var db = Db(out var sent);
        await db.From("people")
            .Order("city", "desc", collate: "tr")
            .Order("name", "asc", collate: "tr")
            .RowsAsync();
        Sends(sent, "get people order city collate tr desc, name collate tr asc", "[]");
    }

    [Fact(DisplayName = "docs: people counted by city in order")]
    public async Task PeopleCountedByCityInOrder()
    {
        using var db = Db(out var sent);
        await db.From("people")
            .Select("city", "count(*)")
            .Group("city")
            .Order("city", "asc", collate: "und")
            .RowsAsync();
        Sends(sent, "get people select city, count(*) group city order city collate und asc", "[]");
    }

    [Fact(DisplayName = "docs: the page of people after a name")]
    public async Task ThePageOfPeopleAfterAName()
    {
        using var db = Db(out var sent);
        await db.From("people")
            .Where("name", ">", "Ada")
            .Order("name")
            .Limit(20)
            .RowsAsync();
        Sends(sent, "get people where name > $1 order name asc limit $2", "[\"Ada\", 20]");
    }

    [Fact(DisplayName = "docs: a user")]
    public async Task AUser()
    {
        using var db = Db(out var sent);
        await db.From("users")
            .InsertAsync(new { email = "Alice@Example.com", name = "İsmail" });
        Sends(sent, "put users {email: $1, name: $2}", "[\"Alice@Example.com\", \"İsmail\"]");
    }

    [Fact(DisplayName = "docs: a user by an email in any case")]
    public async Task AUserByAnEmailInAnyCase()
    {
        using var db = Db(out var sent);
        await db.From("users").Where("email", "alice@example.COM").RowsAsync();
        Sends(sent, "get users where email = $1", "[\"alice@example.COM\"]");
    }

    [Fact(DisplayName = "docs: a user whose email is taken")]
    public async Task AUserWhoseEmailIsTaken()
    {
        using var db = Db(out var sent);
        await db.From("users").InsertAsync(new { email = "ALICE@example.com" });
        Sends(sent, "put users {email: $1}", "[\"ALICE@example.com\"]");
    }

    [Fact(DisplayName = "docs: a user unless the email is taken")]
    public async Task AUserUnlessTheEmailIsTaken()
    {
        using var db = Db(out var sent);
        await db.From("users")
            .InsertAsync(new { email = "alice@example.com" }, ifAbsent: true);
        Sends(sent, "put users {email: $1} if absent", "[\"alice@example.com\"]");
    }

    [Fact(DisplayName = "docs: a user by a name in any case")]
    public async Task AUserByANameInAnyCase()
    {
        using var db = Db(out var sent);
        await db.From("users").Where("name", "ismail").RowsAsync();
        Sends(sent, "get users where name = $1", "[\"ismail\"]");
    }

    [Fact(DisplayName = "docs: users in Turkish order")]
    public async Task UsersInTurkishOrder()
    {
        using var db = Db(out var sent);
        await db.From("users")
            .Select("name")
            .Order("name")
            .Limit(20)
            .RowsAsync();
        Sends(sent, "get users select name order name asc limit $1", "[20]");
    }

    [Fact(DisplayName = "docs: articles matching words")]
    public async Task ArticlesMatchingWords()
    {
        using var db = Db(out var sent);
        await db.From("articles")
            .Match("body", "ownership")
            .Limit(10)
            .RowsAsync();
        Sends(sent, "get articles match body $1 limit $2", "[\"ownership\", 10]");
    }

    [Fact(DisplayName = "docs: articles matching words since a year")]
    public async Task ArticlesMatchingWordsSinceAYear()
    {
        using var db = Db(out var sent);
        await db.From("articles")
            .Where("year", ">=", 2024)
            .Match("body", "ownership")
            .Limit(10)
            .RowsAsync();
        Sends(sent, "get articles where year >= $1 match body $2 limit $3", "[2024, \"ownership\", 10]");
    }

    [Fact(DisplayName = "docs: articles matching words reranked")]
    public async Task ArticlesMatchingWordsReranked()
    {
        using var db = Db(out var sent);
        await db.From("articles")
            .Match("body", "ownership")
            .Rerank("embed", new[] { 0.9, 0.1, 0 }, candidates: 500)
            .Limit(10)
            .RowsAsync();
        Sends(sent, "get articles match body $1 rerank embed $2 candidates 500 limit $3", "[\"ownership\", [0.9, 0.1, 0], 10]");
    }

    [Fact(DisplayName = "docs: matches with their marks")]
    public async Task MatchesWithTheirMarks()
    {
        using var db = Db(out var sent);
        await db.From("articles")
            .Select("title")
            .Highlight("body")
            .Match("body", "ownership")
            .Limit(10)
            .RowsAsync();
        Sends(sent, "get articles select title, highlight(body) match body $1 limit $2", "[\"ownership\", 10]");
    }

    [Fact(DisplayName = "docs: matches with their marks tagged")]
    public async Task MatchesWithTheirMarksTagged()
    {
        using var db = Db(out var sent);
        await db.From("articles")
            .Select("title")
            .Highlight("body", pre: "<mark>", post: "</mark>")
            .Match("body", "ownership")
            .Limit(10)
            .RowsAsync();
        Sends(sent, "get articles select title, highlight(body, $1, $2) match body $3 limit $4", "[\"<mark>\", \"</mark>\", \"ownership\", 10]");
    }

    [Fact(DisplayName = "docs: matches with a snippet")]
    public async Task MatchesWithASnippet()
    {
        using var db = Db(out var sent);
        await db.From("articles")
            .Select("title")
            .Snippet("body", 20, ellipsis: "…")
            .Match("body", "ownership")
            .Limit(10)
            .RowsAsync();
        Sends(sent, "get articles select title, snippet(body, 20, $1) match body $2 limit $3", "[\"…\", \"ownership\", 10]");
    }

    [Fact(DisplayName = "docs: matches with a snippet tagged")]
    public async Task MatchesWithASnippetTagged()
    {
        using var db = Db(out var sent);
        await db.From("articles")
            .Select("title")
            .Snippet("body", 20, ellipsis: "…", pre: "<b>", post: "</b>")
            .Match("body", "ownership")
            .Limit(10)
            .RowsAsync();
        Sends(sent, "get articles select title, snippet(body, 20, $1, $2, $3) match body $4 limit $5", "[\"…\", \"<b>\", \"</b>\", \"ownership\", 10]");
    }

    [Fact(DisplayName = "docs: a page of products with brands and colours counted")]
    public async Task APageOfProductsWithBrandsAndColoursCounted()
    {
        using var db = Db(out var sent);
        await db.From("products")
            .Where("price", "<", 500)
            .Limit(20)
            .Facet("brand")
            .Facet("color")
            .RowsAsync();
        Sends(sent, "get products where price < $1 limit $2 facet brand, color", "[500, 20]");
    }

    [Fact(DisplayName = "docs: a page of matches with brands and tags counted")]
    public async Task APageOfMatchesWithBrandsAndTagsCounted()
    {
        using var db = Db(out var sent);
        await db.From("products")
            .Where("price", "<", 500)
            .Match("title", "tent")
            .Limit(20)
            .Facet("brand", top: 10)
            .Facet("tags")
            .RowsAsync();
        Sends(sent, "get products where price < $1 match title $2 limit $3 facet brand top $4, tags", "[500, \"tent\", 20, 10]");
    }

    [Fact(DisplayName = "docs: phones counted by brand")]
    public async Task PhonesCountedByBrand()
    {
        using var db = Db(out var sent);
        await db.From("products")
            .Where("category", "phones")
            .Facet("brand")
            .CountAsync();
        Sends(sent, "get products where category = $1 count facet brand", "[\"phones\"]");
    }

    [Fact(DisplayName = "docs: products counted by origin alone")]
    public async Task ProductsCountedByOriginAlone()
    {
        using var db = Db(out var sent);
        await db.From("products")
            .Limit(0)
            .Facet("meta.origin", top: 5)
            .RowsAsync();
        Sends(sent, "get products limit $1 facet meta.origin top $2", "[0, 5]");
    }

    [Fact(DisplayName = "docs: red tents of two brands with their facets")]
    public async Task RedTentsOfTwoBrandsWithTheirFacets()
    {
        using var db = Db(out var sent);
        await db.From("products")
            .Where("category", "tents")
            .Where("brand", "in", new object?[] { "acme", "nova" })
            .Where("color", "red")
            .Order("price")
            .Limit(24)
            .Facet("brand", disjunctive: true)
            .Facet("color", disjunctive: true)
            .Facet("material")
            .RowsAsync();
        Sends(sent, "get products where category = $1 and brand in [$2, $3] and color = $4 order price asc limit $5 facet brand disjunctive, color disjunctive, material", "[\"tents\", \"acme\", \"nova\", \"red\", 24]");
    }

    [Fact(DisplayName = "docs: tents counted by price range")]
    public async Task TentsCountedByPriceRange()
    {
        using var db = Db(out var sent);
        await db.From("products")
            .Where("category", "tents")
            .Limit(24)
            .Facet("price", ranges: [0, 2500, 5000, 10000, 25000], disjunctive: true)
            .RowsAsync();
        Sends(sent, "get products where category = $1 limit $2 facet price ranges [0, 2500, 5000, 10000, 25000] disjunctive", "[\"tents\", 24]");
    }

    [Fact(DisplayName = "docs: the nearest articles")]
    public async Task TheNearestArticles()
    {
        using var db = Db(out var sent);
        await db.From("articles")
            .Near("embed", new[] { 0.9, 0.1, 0 })
            .Limit(10)
            .RowsAsync();
        Sends(sent, "get articles near embed $1 limit $2", "[[0.9, 0.1, 0], 10]");
    }

    [Fact(DisplayName = "docs: the nearest articles since a year")]
    public async Task TheNearestArticlesSinceAYear()
    {
        using var db = Db(out var sent);
        await db.From("articles")
            .Where("year", ">=", 2024)
            .Near("embed", new[] { 0.9, 0.1, 0 }, ef: 200)
            .Limit(10)
            .RowsAsync();
        Sends(sent, "get articles where year >= $1 near embed $2 ef 200 limit $3", "[2024, [0.9, 0.1, 0], 10]");
    }

    [Fact(DisplayName = "docs: the nearest articles exactly")]
    public async Task TheNearestArticlesExactly()
    {
        using var db = Db(out var sent);
        await db.From("articles")
            .Near("embed", new[] { 0.9, 0.1, 0 }, exact: true)
            .Limit(10)
            .RowsAsync();
        Sends(sent, "get articles near embed $1 exact limit $2", "[[0.9, 0.1, 0], 10]");
    }

    [Fact(DisplayName = "docs: the nearest articles filtered")]
    public async Task TheNearestArticlesFiltered()
    {
        using var db = Db(out var sent);
        await db.From("articles")
            .Select("title", "year")
            .Where("year", ">=", 2024)
            .Where("tags", "has", "rust")
            .Where(Cond.Not(Cond.Cmp("title", "~", "draft")))
            .Near("embed", new[] { 0.9, 0.1, 0 }, ef: 128)
            .Limit(10)
            .RowsAsync();
        Sends(sent, "get articles select title, year where year >= $1 and tags has $2 and not (title ~ $3) near embed $4 ef 128 limit $5", "[2024, \"rust\", \"draft\", [0.9, 0.1, 0], 10]");
    }

    [Fact(DisplayName = "docs: a debit that must find the money, or says why")]
    public async Task ADebitThatMustFindTheMoneyOrSaysWhy()
    {
        using var db = Db(out var sent);
        await db.From("accounts")
            .Where("name", "ada")
            .Where("balance", ">=", 30)
            .UpdateAsync(
                new { balance = Computed.Expr("balance - ?", 30) },
                require: 1,
                reason: "insufficient funds"
            );
        Sends(sent, "set accounts {balance: balance - $1} where name = $2 and balance >= $3 require 1 else 'insufficient funds'", "[30, \"ada\", 30]");
    }

    [Fact(DisplayName = "docs: a hold deleted that must be there, or says why")]
    public async Task AHoldDeletedThatMustBeThereOrSaysWhy()
    {
        using var db = Db(out var sent);
        await db.From("holds")
            .Where("order_ref", "o-17")
            .Where("state", "held")
            .DeleteAsync(require: 1, reason: "it's not held");
        Sends(sent, "del holds where order_ref = $1 and state = $2 require 1 else 'it''s not held'", "[\"o-17\", \"held\"]");
    }

    [Fact(DisplayName = "docs: a coupon that must still be usable, or says why")]
    public async Task ACouponThatMustStillBeUsableOrSaysWhy()
    {
        using var db = Db(out var sent);
        await db.From("coupons")
            .Where("code", "X1")
            .Where(Cond.Raw("uses < max_uses"))
            .Limit(1)
            .Require(1, reason: "coupon used up")
            .RowsAsync();
        Sends(sent, "get coupons where code = $1 and uses < max_uses limit $2 require 1 else 'coupon used up'", "[\"X1\", 1]");
    }

    [Fact(DisplayName = "docs: a product that must be there at its price")]
    public async Task AProductThatMustBeThereAtItsPrice()
    {
        using var db = Db(out var sent);
        await db.From("products")
            .Where("sku", "tee")
            .Where("price", 1200)
            .Where("stock", ">", 0)
            .Require(1)
            .RowsAsync();
        Sends(sent, "get products where sku = $1 and price = $2 and stock > $3 require 1", "[\"tee\", 1200, 0]");
    }

    [Fact(DisplayName = "docs: a coupon that must still be usable")]
    public async Task ACouponThatMustStillBeUsable()
    {
        using var db = Db(out var sent);
        await db.From("coupons")
            .Where("code", "X1")
            .Where(Cond.Raw("uses < max_uses"))
            .Limit(1)
            .Require(1)
            .RowsAsync();
        Sends(sent, "get coupons where code = $1 and uses < max_uses limit $2 require 1", "[\"X1\", 1]");
    }

    [Fact(DisplayName = "docs: no hold that must be there")]
    public async Task NoHoldThatMustBeThere()
    {
        using var db = Db(out var sent);
        await db.From("holds")
            .Where("order_ref", "o-17")
            .Where("state", "held")
            .Require(0)
            .RowsAsync();
        Sends(sent, "get holds where order_ref = $1 and state = $2 require 0", "[\"o-17\", \"held\"]");
    }

    [Fact(DisplayName = "docs: two jobs queued")]
    public async Task TwoJobsQueued()
    {
        using var db = Db(out var sent);
        await db.From("jobs")
            .InsertAsync(new object[] { new { payload = new { to = "ada" }, run_at = Computed.Expr("now()"), attempts = 0 }, new { payload = new { to = "bo" }, run_at = Computed.Expr("now()"), attempts = 0 } });
        Sends(sent, "put jobs [{payload: $1, run_at: now(), attempts: $2}, {payload: $3, run_at: now(), attempts: $4}]", "[{\"to\": \"ada\"}, 0, {\"to\": \"bo\"}, 0]");
    }

    [Fact(DisplayName = "docs: the ten oldest ready jobs claimed")]
    public async Task TheTenOldestReadyJobsClaimed()
    {
        using var db = Db(out var sent);
        await db.From("jobs")
            .Where(Cond.Raw("run_at <= now()"))
            .Order("run_at")
            .Limit(10)
            .UpdateAsync(
                new { owner = "w1", run_at = Computed.Expr("now() + ?", 30000), attempts = Computed.Expr("attempts + 1") },
                returning: ["*"]
            );
        Sends(sent, "set jobs {owner: $1, run_at: now() + $2, attempts: attempts + 1} where run_at <= now() order run_at asc limit $3 returning *", "[\"w1\", 30000, 10]");
    }

    [Fact(DisplayName = "docs: a job acknowledged")]
    public async Task AJobAcknowledged()
    {
        using var db = Db(out var sent);
        await db.From("jobs")
            .Where("id", 1)
            .Where("owner", "w1")
            .DeleteAsync(require: 1);
        Sends(sent, "del jobs where id = $1 and owner = $2 require 1", "[1, \"w1\"]");
    }

    [Fact(DisplayName = "docs: ready jobs popped")]
    public async Task ReadyJobsPopped()
    {
        using var db = Db(out var sent);
        await db.From("jobs")
            .Where(Cond.Raw("run_at <= now()"))
            .Order("run_at")
            .Limit(10)
            .DeleteAsync(returning: ["id", "payload.to"]);
        Sends(sent, "del jobs where run_at <= now() order run_at asc limit $1 returning id, payload.to", "[10]");
    }

    [Fact(DisplayName = "docs: an article set by id")]
    public async Task AnArticleSetById()
    {
        using var db = Db(out var sent);
        await db.From("articles")
            .Where("id", 42)
            .UpdateAsync(new { year = 2025 });
        Sends(sent, "set articles {year: $1} where id = $2", "[2025, 42]");
    }

    [Fact(DisplayName = "docs: an article's views counted")]
    public async Task AnArticleSViewsCounted()
    {
        using var db = Db(out var sent);
        await db.From("articles")
            .Where("id", 42)
            .UpdateAsync(new { views = Computed.Inc(1) });
        Sends(sent, "set articles {views: coalesce(views, 0) + $1} where id = $2", "[1, 42]");
    }

    [Fact(DisplayName = "docs: the articles before a year deleted")]
    public async Task TheArticlesBeforeAYearDeleted()
    {
        using var db = Db(out var sent);
        await db.From("articles").Where("year", "<", 2000).DeleteAsync();
        Sends(sent, "del articles where year < $1", "[2000]");
    }

    [Fact(DisplayName = "docs: a page of documents with their comments")]
    public async Task APageOfDocumentsWithTheirComments()
    {
        using var db = Db(out var sent);
        await db.From("docs")
            .Where("year", ">=", 2024)
            .Where(Cond.Or(Cond.Cmp("lang", "=", "tr"), Cond.Cmp("tags", "has", "rust")))
            .Order("title", "asc", collate: "tr")
            .Limit(20)
            .Lookup("comments", on: "doc_id", limit: 3)
            .RowsAsync();
        Sends(sent, "get docs where year >= $1 and (lang = $2 or tags has $3) order title collate tr asc limit $4 lookup comments on doc_id limit $5", "[2024, \"tr\", \"rust\", 20, 3]");
    }

    [Fact(DisplayName = "docs: documents matching a misspelt word")]
    public async Task DocumentsMatchingAMisspeltWord()
    {
        using var db = Db(out var sent);
        await db.From("docs")
            .Where("year", ">=", 2024)
            .Match("title", "databse", fuzzy: 2)
            .Limit(10)
            .RowsAsync();
        Sends(sent, "get docs where year >= $1 match title $2 fuzzy 2 limit $3", "[2024, \"databse\", 10]");
    }

    [Fact(DisplayName = "docs: the document of a slug")]
    public async Task TheDocumentOfASlug()
    {
        using var db = Db(out var sent);
        await db.From("docs").Where("slug", "intro").FirstAsync();
        Sends(sent, "get docs where slug = $1 limit $2", "[\"intro\", 1]");
    }

    [Fact(DisplayName = "docs: documents counted since a year")]
    public async Task DocumentsCountedSinceAYear()
    {
        using var db = Db(out var sent);
        await db.From("docs").Where("year", ">=", 2024).CountAsync();
        Sends(sent, "get docs where year >= $1 count", "[2024]");
    }

    [Fact(DisplayName = "docs: a document inserted")]
    public async Task ADocumentInserted()
    {
        using var db = Db(out var sent);
        await db.From("docs")
            .InsertAsync(new { slug = "intro", title = "Ownership in Rust", year = 2026 });
        Sends(sent, "put docs {slug: $1, title: $2, year: $3}", "[\"intro\", \"Ownership in Rust\", 2026]");
    }

    [Fact(DisplayName = "docs: a document inserted unless its slug is held")]
    public async Task ADocumentInsertedUnlessItsSlugIsHeld()
    {
        using var db = Db(out var sent);
        await db.From("docs")
            .InsertAsync(
                new { slug = "intro", title = "Ownership in Rust" },
                ifAbsent: true
            );
        Sends(sent, "put docs {slug: $1, title: $2} if absent", "[\"intro\", \"Ownership in Rust\"]");
    }

    [Fact(DisplayName = "docs: a document's views counted")]
    public async Task ADocumentSViewsCounted()
    {
        using var db = Db(out var sent);
        await db.From("docs")
            .Where("slug", "intro")
            .UpdateAsync(new { views = Computed.Inc(1) });
        Sends(sent, "set docs {views: coalesce(views, 0) + $1} where slug = $2", "[1, \"intro\"]");
    }

    [Fact(DisplayName = "docs: the documents before a year deleted")]
    public async Task TheDocumentsBeforeAYearDeleted()
    {
        using var db = Db(out var sent);
        await db.From("docs").Where("year", "<", 2000).DeleteAsync();
        Sends(sent, "del docs where year < $1", "[2000]");
    }

    [Fact(DisplayName = "escapes: every character a literal escapes, in a filter")]
    public async Task EscapesEveryCharacterALiteralEscapesInAFilter()
    {
        using var db = Db(out var sent);
        await db.From("docs")
            .Where(
                "title",
                "\u0000\u0001\u0009\u000a\u000d\u001f\u0085\u2028\u2029 \"'\\$`${x}$1\\(x) …"
            )
            .RowsAsync();
        Sends(sent, "get docs where title = $1", "[\"\\u0000\\u0001\\t\\n\\r\\u001f\u0085\u2028\u2029 \\\"'\\\\$`${x}$1\\\\(x) …\"]");
    }

    [Fact(DisplayName = "escapes: line ends, in a document")]
    public async Task EscapesLineEndsInADocument()
    {
        using var db = Db(out var sent);
        await db.From("docs")
            .Where("id", 1)
            .UpdateAsync(new { title = "one\u2028two\u000d\u000athree\u0085four\u2029", note = "\u0000\u0001\u0009\u000a\u000d\u001f\u0085\u2028\u2029 \"'\\$`${x}$1\\(x) …" });
        Sends(sent, "set docs {title: $1, note: $2} where id = $3", "[\"one\u2028two\\r\\nthree\u0085four\u2029\", \"\\u0000\\u0001\\t\\n\\r\\u001f\u0085\u2028\u2029 \\\"'\\\\$`${x}$1\\\\(x) …\", 1]");
    }
}
