// Written by site/builder_tabs.py (make docs-builder): do not edit.

// The code of each builder tab on the site's FenecQL pages, as the page
// shows it, run against a transport that records and held to the statement
// its case of integrations/builder-golden.json makes: a tab shows code this
// SDK runs. make docs-builder writes the file again as the pages' tabs
// change, and site/builder_tabs.py --check fails while it is behind them.

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

    [Fact(DisplayName = "docs: articles counted since a year")]
    public async Task ArticlesCountedSinceAYear()
    {
        using var db = Db(out var sent);
        await db.From("articles").Where("year", ">=", 2024).CountAsync();
        Sends(sent, "get articles where year >= $1 count", "[2024]");
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
}
