// Written by site/builder_tabs.py (make docs-builder): do not edit.

// The code of each builder tab on the site's FenecQL pages, as the page
// shows it, run against a transport that records and held to the statement
// its case of integrations/builder-golden.json makes: a tab shows code this
// SDK runs. make docs-builder writes the file again as the pages' tabs
// change, and site/builder_tabs.py --check fails while it is behind them.

import Testing

@testable import FenecDB

@Suite struct DocsBuilder {
    /// What the tabs call `db`: queries bound to a recorder, as a database's are to its engine.
    struct Recording: Sendable {
        let sent = Sent()

        func from(_ collection: String) throws -> Query {
            try Query.from(collection).bind { [sent] text, params in
                await sent.record(text, params)
                if text.hasPrefix("put ") || text.hasPrefix("set ") || text.hasPrefix("del ") { return .affected(0) }
                if text.hasSuffix(" count") { return .rows(columns: ["count"], rows: [["count": 0]]) }
                return .rows(columns: [], rows: [])
            }
        }

        func sends(_ text: String, _ params: String) async throws {
            let got = await sent.last
            #expect(got?.0 == text)
            #expect(Golden.same(.array(got?.1 ?? []), try Value.parse(params)), "\(Value.array(got?.1 ?? []).json)")
        }
    }

    actor Sent {
        var last: (String, [Value])?
        func record(_ text: String, _ params: [Value]) { last = (text, params) }
    }

    @Test("docs: aggregates over every order") func aggregatesOverEveryOrder() async throws {
        let db = Recording()
        _ = try await db.from("orders")
            .select("count(*)", "sum(total)", "avg(total)", "min(at)", "max(at)")
            .rows()
        try await db.sends("get orders select count(*), sum(total), avg(total), min(at), max(at)", "[]")
    }

    @Test("docs: aggregates of each status") func aggregatesOfEachStatus() async throws {
        let db = Recording()
        _ = try await db.from("orders")
            .select("status", "count(*)", "avg(total)")
            .where("year", 2026)
            .group("status")
            .rows()
        try await db.sends("get orders select status, count(*), avg(total) where year = $1 group status", "[2026]")
    }

    @Test("docs: the ten customers who spent most") func theTenCustomersWhoSpentMost() async throws {
        let db = Recording()
        _ = try await db.from("orders")
            .select("customer", "sum(total)")
            .group("customer")
            .order("sum(total)", "desc")
            .limit(10)
            .rows()
        try await db.sends("get orders select customer, sum(total) group customer order sum(total) desc limit $1", "[10]")
    }

    @Test("docs: distinct users an hour and event") func distinctUsersAnHourAndEvent() async throws {
        let db = Recording()
        _ = try await db.from("events")
            .select(
                .bucket("at", "1h").as("hour"),
                "name",
                .countDistinct("user").as("users")
            )
            .group("hour", "name")
            .rows()
        try await db.sends("get events select bucket(at, 1h) as hour, name, count(distinct user) as users group hour, name", "[]")
    }

    @Test("docs: one-minute bars of a symbol") func oneMinuteBarsOfASymbol() async throws {
        let db = Recording()
        _ = try await db.from("ticks")
            .select(
                .bucket("at", "1m").as("bar"),
                .first("px", by: "at").as("open"),
                .expr("max(px)").as("high"),
                .expr("min(px)").as("low"),
                .last("px", by: "at").as("close"),
                .expr("sum(qty)").as("volume")
            )
            .where("sym", "S07")
            .group("bar")
            .rows()
        try await db.sends("get ticks select bucket(at, 1m) as bar, first(px by at) as open, max(px) as high, min(px) as low, last(px by at) as close, sum(qty) as volume where sym = $1 group bar", "[\"S07\"]")
    }

    @Test("docs: vwap of each symbol") func vwapOfEachSymbol() async throws {
        let db = Recording()
        _ = try await db.from("ticks")
            .select("sym", .expr("sum(px * qty) / sum(qty)").as("vwap"))
            .group("sym")
            .rows()
        try await db.sends("get ticks select sym, sum(px * qty) / sum(qty) as vwap group sym", "[]")
    }

    @Test("docs: totals of a json path") func totalsOfAJsonPath() async throws {
        let db = Recording()
        _ = try await db.from("events")
            .select("props.plan", "sum(props.v)")
            .group("props.plan")
            .rows()
        try await db.sends("get events select props.plan, sum(props.v) group props.plan", "[]")
    }

    @Test("docs: the three statuses that took most") func theThreeStatusesThatTookMost() async throws {
        let db = Recording()
        _ = try await db.from("orders")
            .select("status", "count(*)", "sum(total)")
            .where("year", 2026)
            .group("status")
            .order("sum(total)", "desc")
            .limit(3)
            .rows()
        try await db.sends("get orders select status, count(*), sum(total) where year = $1 group status order sum(total) desc limit $2", "[2026, 3]")
    }

    @Test("docs: one-minute bars since a time") func oneMinuteBarsSinceATime() async throws {
        let db = Recording()
        _ = try await db.from("ticks")
            .select(
                .bucket("at", "1m").as("bar"),
                .first("px", by: "at").as("open"),
                .expr("max(px)").as("high"),
                .expr("min(px)").as("low"),
                .last("px", by: "at").as("close"),
                .expr("sum(qty)").as("volume")
            )
            .where("sym", "S07")
            .where("at", ">=", "2026-05-03T09:00:00Z")
            .group("bar")
            .rows()
        try await db.sends("get ticks select bucket(at, 1m) as bar, first(px by at) as open, max(px) as high, min(px) as low, last(px by at) as close, sum(qty) as volume where sym = $1 and at >= $2 group bar", "[\"S07\", \"2026-05-03T09:00:00Z\"]")
    }

    @Test("docs: the newest articles") func theNewestArticles() async throws {
        let db = Recording()
        _ = try await db.from("articles")
            .order("published", "desc")
            .limit(20)
            .rows()
        try await db.sends("get articles order published desc limit $1", "[20]")
    }

    @Test("docs: the newest articles since a day") func theNewestArticlesSinceADay() async throws {
        let db = Recording()
        _ = try await db.from("articles")
            .where("published", ">=", "2026-09-01")
            .order("published", "desc")
            .limit(20)
            .rows()
        try await db.sends("get articles where published >= $1 order published desc limit $2", "[\"2026-09-01\", 20]")
    }

    @Test("docs: articles counted over a range of titles") func articlesCountedOverARangeOfTitles() async throws {
        let db = Recording()
        _ = try await db.from("articles")
            .where("title", ">=", "k")
            .where("title", "<", "l")
            .count()
        try await db.sends("get articles where title >= $1 and title < $2 count", "[\"k\", \"l\"]")
    }

    @Test("docs: the nearest articles since a day") func theNearestArticlesSinceADay() async throws {
        let db = Recording()
        _ = try await db.from("articles")
            .where("published", ">=", "2026-01-01")
            .near("embed", [0.9, 0.1, 0])
            .limit(10)
            .rows()
        try await db.sends("get articles where published >= $1 near embed $2 limit $3", "[\"2026-01-01\", [0.9, 0.1, 0], 10]")
    }

    @Test("docs: products with their newest reviews") func productsWithTheirNewestReviews() async throws {
        let db = Recording()
        _ = try await db.from("products")
            .limit(20)
            .lookup(
                "reviews",
                on: "product_id",
                order: [SortKey("created", "desc")],
                limit: 3
            )
            .rows()
        try await db.sends("get products limit $1 lookup reviews on product_id order created desc limit $2", "[20, 3]")
    }

    @Test("docs: shops, their orders and the orders' lines") func shopsTheirOrdersAndTheOrdersLines() async throws {
        let db = Recording()
        _ = try await db.from("shops")
            .limit(20)
            .lookup("orders", on: "shop_id", limit: 3)
            .lookup("lines", on: "order_id", limit: 5)
            .rows()
        try await db.sends("get shops limit $1 lookup orders on shop_id limit $2 lookup lines on order_id limit $3", "[20, 3, 5]")
    }

    @Test("docs: products with a five-star review") func productsWithAFiveStarReview() async throws {
        let db = Recording()
        _ = try await db.from("products")
            .limit(20)
            .lookup("reviews", on: "product_id", where: ["stars": 5], required: true)
            .rows()
        try await db.sends("get products limit $1 lookup reviews on product_id required where stars = $2", "[20, 5]")
    }

    @Test("docs: products with a five-star review counted") func productsWithAFiveStarReviewCounted() async throws {
        let db = Recording()
        _ = try await db.from("products")
            .lookup("reviews", on: "product_id", where: ["stars": 5], required: true)
            .count()
        try await db.sends("get products count lookup reviews on product_id required where stars = $1", "[5]")
    }

    @Test("docs: articles counted since a year") func articlesCountedSinceAYear() async throws {
        let db = Recording()
        _ = try await db.from("articles").where("year", ">=", 2024).count()
        try await db.sends("get articles where year >= $1 count", "[2024]")
    }

    @Test("docs: articles matching words") func articlesMatchingWords() async throws {
        let db = Recording()
        _ = try await db.from("articles")
            .match("body", "ownership")
            .limit(10)
            .rows()
        try await db.sends("get articles match body $1 limit $2", "[\"ownership\", 10]")
    }

    @Test("docs: articles matching words since a year") func articlesMatchingWordsSinceAYear() async throws {
        let db = Recording()
        _ = try await db.from("articles")
            .where("year", ">=", 2024)
            .match("body", "ownership")
            .limit(10)
            .rows()
        try await db.sends("get articles where year >= $1 match body $2 limit $3", "[2024, \"ownership\", 10]")
    }

    @Test("docs: articles matching words reranked") func articlesMatchingWordsReranked() async throws {
        let db = Recording()
        _ = try await db.from("articles")
            .match("body", "ownership")
            .rerank("embed", [0.9, 0.1, 0], candidates: 500)
            .limit(10)
            .rows()
        try await db.sends("get articles match body $1 rerank embed $2 candidates 500 limit $3", "[\"ownership\", [0.9, 0.1, 0], 10]")
    }

    @Test("docs: matches with their marks") func matchesWithTheirMarks() async throws {
        let db = Recording()
        _ = try await db.from("articles")
            .select("title")
            .highlight("body")
            .match("body", "ownership")
            .limit(10)
            .rows()
        try await db.sends("get articles select title, highlight(body) match body $1 limit $2", "[\"ownership\", 10]")
    }

    @Test("docs: matches with their marks tagged") func matchesWithTheirMarksTagged() async throws {
        let db = Recording()
        _ = try await db.from("articles")
            .select("title")
            .highlight("body", pre: "<mark>", post: "</mark>")
            .match("body", "ownership")
            .limit(10)
            .rows()
        try await db.sends("get articles select title, highlight(body, $1, $2) match body $3 limit $4", "[\"<mark>\", \"</mark>\", \"ownership\", 10]")
    }

    @Test("docs: matches with a snippet") func matchesWithASnippet() async throws {
        let db = Recording()
        _ = try await db.from("articles")
            .select("title")
            .snippet("body", 20, ellipsis: "…")
            .match("body", "ownership")
            .limit(10)
            .rows()
        try await db.sends("get articles select title, snippet(body, 20, $1) match body $2 limit $3", "[\"…\", \"ownership\", 10]")
    }

    @Test("docs: matches with a snippet tagged") func matchesWithASnippetTagged() async throws {
        let db = Recording()
        _ = try await db.from("articles")
            .select("title")
            .snippet("body", 20, ellipsis: "…", pre: "<b>", post: "</b>")
            .match("body", "ownership")
            .limit(10)
            .rows()
        try await db.sends("get articles select title, snippet(body, 20, $1, $2, $3) match body $4 limit $5", "[\"…\", \"<b>\", \"</b>\", \"ownership\", 10]")
    }

    @Test("docs: tents counted by price range") func tentsCountedByPriceRange() async throws {
        let db = Recording()
        _ = try await db.from("products")
            .where("category", "tents")
            .limit(24)
            .facet("price", ranges: [0, 2500, 5000, 10000, 25000], disjunctive: true)
            .rows()
        try await db.sends("get products where category = $1 limit $2 facet price ranges [0, 2500, 5000, 10000, 25000] disjunctive", "[\"tents\", 24]")
    }

    @Test("docs: the nearest articles") func theNearestArticles() async throws {
        let db = Recording()
        _ = try await db.from("articles")
            .near("embed", [0.9, 0.1, 0])
            .limit(10)
            .rows()
        try await db.sends("get articles near embed $1 limit $2", "[[0.9, 0.1, 0], 10]")
    }

    @Test("docs: the nearest articles since a year") func theNearestArticlesSinceAYear() async throws {
        let db = Recording()
        _ = try await db.from("articles")
            .where("year", ">=", 2024)
            .near("embed", [0.9, 0.1, 0], ef: 200)
            .limit(10)
            .rows()
        try await db.sends("get articles where year >= $1 near embed $2 ef 200 limit $3", "[2024, [0.9, 0.1, 0], 10]")
    }

    @Test("docs: the nearest articles exactly") func theNearestArticlesExactly() async throws {
        let db = Recording()
        _ = try await db.from("articles")
            .near("embed", [0.9, 0.1, 0], exact: true)
            .limit(10)
            .rows()
        try await db.sends("get articles near embed $1 exact limit $2", "[[0.9, 0.1, 0], 10]")
    }

    @Test("docs: the nearest articles filtered") func theNearestArticlesFiltered() async throws {
        let db = Recording()
        _ = try await db.from("articles")
            .select("title", "year")
            .where("year", ">=", 2024)
            .where("tags", "has", "rust")
            .where(.not(.cmp("title", "~", "draft")))
            .near("embed", [0.9, 0.1, 0], ef: 128)
            .limit(10)
            .rows()
        try await db.sends("get articles select title, year where year >= $1 and tags has $2 and not (title ~ $3) near embed $4 ef 128 limit $5", "[2024, \"rust\", \"draft\", [0.9, 0.1, 0], 10]")
    }

    @Test("docs: a product that must be there at its price") func aProductThatMustBeThereAtItsPrice() async throws {
        let db = Recording()
        _ = try await db.from("products")
            .where("sku", "tee")
            .where("price", 1200)
            .where("stock", ">", 0)
            .require(1)
            .rows()
        try await db.sends("get products where sku = $1 and price = $2 and stock > $3 require 1", "[\"tee\", 1200, 0]")
    }

    @Test("docs: a coupon that must still be usable") func aCouponThatMustStillBeUsable() async throws {
        let db = Recording()
        _ = try await db.from("coupons")
            .where("code", "X1")
            .where(.raw("uses < max_uses"))
            .limit(1)
            .require(1)
            .rows()
        try await db.sends("get coupons where code = $1 and uses < max_uses limit $2 require 1", "[\"X1\", 1]")
    }

    @Test("docs: no hold that must be there") func noHoldThatMustBeThere() async throws {
        let db = Recording()
        _ = try await db.from("holds")
            .where("order_ref", "o-17")
            .where("state", "held")
            .require(0)
            .rows()
        try await db.sends("get holds where order_ref = $1 and state = $2 require 0", "[\"o-17\", \"held\"]")
    }

    @Test("docs: an article set by id") func anArticleSetById() async throws {
        let db = Recording()
        _ = try await db.from("articles")
            .where("id", 42)
            .update(["year": 2025] as Value)
        try await db.sends("set articles {year: $1} where id = $2", "[2025, 42]")
    }

    @Test("docs: an article's views counted") func anArticleSViewsCounted() async throws {
        let db = Recording()
        _ = try await db.from("articles")
            .where("id", 42)
            .update(["views": .inc(1)] as Value)
        try await db.sends("set articles {views: coalesce(views, 0) + $1} where id = $2", "[1, 42]")
    }

    @Test("docs: the articles before a year deleted") func theArticlesBeforeAYearDeleted() async throws {
        let db = Recording()
        _ = try await db.from("articles").where("year", "<", 2000).delete()
        try await db.sends("del articles where year < $1", "[2000]")
    }

    @Test("escapes: every character a literal escapes, in a filter") func escapesEveryCharacterALiteralEscapesInAFilter() async throws {
        let db = Recording()
        _ = try await db.from("docs")
            .where(
                "title",
                "\u{0}\u{1}\u{9}\u{a}\u{d}\u{1f}\u{85}\u{2028}\u{2029} \"'\\$`${x}$1\\(x) …"
            )
            .rows()
        try await db.sends("get docs where title = $1", "[\"\\u0000\\u0001\\t\\n\\r\\u001f\u{85}\u{2028}\u{2029} \\\"'\\\\$`${x}$1\\\\(x) …\"]")
    }

    @Test("escapes: line ends, in a document") func escapesLineEndsInADocument() async throws {
        let db = Recording()
        _ = try await db.from("docs")
            .where("id", 1)
            .update(["title": "one\u{2028}two\u{d}\u{a}three\u{85}four\u{2029}", "note": "\u{0}\u{1}\u{9}\u{a}\u{d}\u{1f}\u{85}\u{2028}\u{2029} \"'\\$`${x}$1\\(x) …"] as Value)
        try await db.sends("set docs {title: $1, note: $2} where id = $3", "[\"one\u{2028}two\\r\\nthree\u{85}four\u{2029}\", \"\\u0000\\u0001\\t\\n\\r\\u001f\u{85}\u{2028}\u{2029} \\\"'\\\\$`${x}$1\\\\(x) …\", 1]")
    }
}
