// Written by site/builder_tabs.py (make docs-builder): do not edit.

// The code of each builder tab on the site, as the page shows it, run
// against a transport that records and held to the statement its case of
// integrations/builder-golden.json makes: a tab shows code this SDK runs.
// make docs-builder writes the file again as the pages' tabs change, and
// site/builder_tabs.py --check fails while it is behind them.

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

    @Test("docs: an owner's newest posts") func anOwnerSNewestPosts() async throws {
        let db = Recording()
        _ = try await db.from("posts")
            .where("owner", 7)
            .order("created", "desc")
            .limit(20)
            .rows()
        try await db.sends("get posts where owner = $1 order created desc limit $2", "[7, 20]")
    }

    @Test("docs: three owners' newest posts") func threeOwnersNewestPosts() async throws {
        let db = Recording()
        _ = try await db.from("posts")
            .where("owner", "in", [7, 8, 9])
            .order("created", "desc")
            .limit(20)
            .rows()
        try await db.sends("get posts where owner in [$1, $2, $3] order created desc limit $4", "[7, 8, 9, 20]")
    }

    @Test("docs: an owner's posts counted since a time") func anOwnerSPostsCountedSinceATime() async throws {
        let db = Recording()
        _ = try await db.from("posts")
            .where("owner", 7)
            .where("created", ">=", "2026-10-01")
            .count()
        try await db.sends("get posts where owner = $1 and created >= $2 count", "[7, \"2026-10-01\"]")
    }

    @Test("docs: a session seen") func aSessionSeen() async throws {
        let db = Recording()
        _ = try await db.from("sessions")
            .insert(["user": "ada", "seen": .expr("now()")] as Value)
        try await db.sends("put sessions {user: $1, seen: now()}", "[\"ada\"]")
    }

    @Test("docs: a session seen again") func aSessionSeenAgain() async throws {
        let db = Recording()
        _ = try await db.from("sessions")
            .where("user", "ada")
            .update(["seen": .expr("now()")] as Value)
        try await db.sends("set sessions {seen: now()} where user = $1", "[\"ada\"]")
    }

    @Test("docs: a user's sessions") func aUserSSessions() async throws {
        let db = Recording()
        _ = try await db.from("sessions").where("user", "ada").rows()
        try await db.sends("get sessions where user = $1", "[\"ada\"]")
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

    @Test("docs: products with their reviews") func productsWithTheirReviews() async throws {
        let db = Recording()
        _ = try await db.from("products").lookup("reviews", on: "product_id").rows()
        try await db.sends("get products lookup reviews on product_id", "[]")
    }

    @Test("docs: the names of dear products with their good reviews") func theNamesOfDearProductsWithTheirGoodReviews() async throws {
        let db = Recording()
        _ = try await db.from("products")
            .select("name")
            .where("price", ">=", 10000)
            .lookup(
                "reviews",
                on: "product_id",
                select: ["body"],
                where: .cmp("stars", "gte", 4),
                limit: 3
            )
            .rows()
        try await db.sends("get products select name where price >= $1 lookup reviews on product_id select body where stars >= $2 limit $3", "[10000, 4, 3]")
    }

    @Test("docs: products with their tags by sku") func productsWithTheirTagsBySku() async throws {
        let db = Recording()
        _ = try await db.from("products")
            .lookup("tags", on: "sku", parentKey: "sku")
            .rows()
        try await db.sends("get products lookup tags on sku = sku", "[]")
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

    @Test("docs: orders with their lines by a key of two fields") func ordersWithTheirLinesByAKeyOfTwoFields() async throws {
        let db = Recording()
        _ = try await db.from("orders")
            .where("org", 7)
            .limit(20)
            .lookup(
                "lines",
                on: ["org", "order"],
                parentKey: ["org", "id"],
                order: ["line"],
                limit: 5
            )
            .rows()
        try await db.sends("get orders where org = $1 limit $2 lookup lines on (org, order) = (org, id) order line asc limit $3", "[7, 20, 5]")
    }

    @Test("docs: shops with their orders folded") func shopsWithTheirOrdersFolded() async throws {
        let db = Recording()
        _ = try await db.from("shops")
            .limit(20)
            .lookup(
                "orders",
                on: "shop",
                select: [.expr("count(*)").as("n"), .expr("sum(total)").as("revenue")]
            )
            .rows()
        try await db.sends("get shops limit $1 lookup orders on shop select count(*) as n, sum(total) as revenue", "[20]")
    }

    @Test("docs: couriers with their latest fifty deliveries folded") func couriersWithTheirLatestFiftyDeliveriesFolded() async throws {
        let db = Recording()
        _ = try await db.from("couriers")
            .limit(20)
            .lookup(
                "deliveries",
                on: "courier",
                select: [.expr("count(*)").as("n"), .expr("avg(km)").as("km")],
                order: [SortKey("at", "desc")],
                limit: 50
            )
            .rows()
        try await db.sends("get couriers limit $1 lookup deliveries on courier select count(*) as n, avg(km) as km order at desc limit $2", "[20, 50]")
    }

    @Test("docs: each shop's three best sellers") func eachShopSThreeBestSellers() async throws {
        let db = Recording()
        _ = try await db.from("shops")
            .lookup(
                "lines",
                on: "shop",
                select: ["product", .expr("sum(qty)").as("sold")],
                group: ["product"],
                order: [SortKey("sold", "desc")],
                limit: 3
            )
            .rows()
        try await db.sends("get shops lookup lines on shop select product, sum(qty) as sold group product order sold desc limit $1", "[3]")
    }

    @Test("docs: the busiest couriers with their names") func theBusiestCouriersWithTheirNames() async throws {
        let db = Recording()
        _ = try await db.from("orders")
            .select("courier", .expr("count(*)").as("n"))
            .where("at", ">=", "2026-10-01")
            .group("courier")
            .order("n", "desc")
            .limit(10)
            .lookup("couriers", on: "user", parentKey: "courier", select: ["name"])
            .rows()
        try await db.sends("get orders select courier, count(*) as n where at >= $1 group courier order n desc limit $2 lookup couriers on user = courier select name", "[\"2026-10-01\", 10]")
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

    @Test("docs: articles a page at a time") func articlesAPageAtATime() async throws {
        let db = Recording()
        _ = try await db.from("articles")
            .where("year", 2024)
            .order("published", "desc")
            .limit(20)
            .offset(40)
            .rows()
        try await db.sends("get articles where year = $1 order published desc limit $2 offset $3", "[2024, 20, 40]")
    }

    @Test("docs: a page of matches with its brands counted") func aPageOfMatchesWithItsBrandsCounted() async throws {
        let db = Recording()
        _ = try await db.from("products")
            .match("title", "tent")
            .limit(20)
            .facet("brand", top: 10)
            .rows()
        try await db.sends("get products match title $1 limit $2 facet brand top $3", "[\"tent\", 20, 10]")
    }

    @Test("docs: ready jobs claimed a page at a time") func readyJobsClaimedAPageAtATime() async throws {
        let db = Recording()
        _ = try await db.from("jobs")
            .where(.raw("run_at <= now()"))
            .order("run_at")
            .limit(10)
            .updateReturning(["owner": "w1"] as Value)
        try await db.sends("set jobs {owner: $1} where run_at <= now() order run_at asc limit $2 returning *", "[\"w1\", 10]")
    }

    @Test("docs: a column worked out") func aColumnWorkedOut() async throws {
        let db = Recording()
        _ = try await db.from("ticks")
            .select("sym", .expr("px * qty").as("notional"))
            .where("qty", ">", 100)
            .rows()
        try await db.sends("get ticks select sym, px * qty as notional where qty > $1", "[100]")
    }

    @Test("docs: columns named and worked out") func columnsNamedAndWorkedOut() async throws {
        let db = Recording()
        _ = try await db.from("ticks")
            .select(
                .expr("px").as("price"),
                .bucket("at", "1m").as("minute"),
                .expr("greatest(px, ?)", 100).as("floor")
            )
            .rows()
        try await db.sends("get ticks select px as price, bucket(at, 1m) as minute, greatest(px, $1) as floor", "[100]")
    }

    @Test("docs: a column of a case") func aColumnOfACase() async throws {
        let db = Recording()
        _ = try await db.from("orders")
            .select(
                "id",
                .expr("case when total >= ? then ? else ? end", 100, "large", "small").as("size")
            )
            .rows()
        try await db.sends("get orders select id, case when total >= $1 then $2 else $3 end as size", "[100, \"large\", \"small\"]")
    }

    @Test("docs: articles counted since a year") func articlesCountedSinceAYear() async throws {
        let db = Recording()
        _ = try await db.from("articles").where("year", ">=", 2024).count()
        try await db.sends("get articles where year >= $1 count", "[2024]")
    }

    @Test("docs: a document with a json field") func aDocumentWithAJsonField() async throws {
        let db = Recording()
        _ = try await db.from("docs")
            .insert(["title": "a", "meta": ["lang": "tr", "source": ["site": "x", "rank": 3]]] as Value)
        try await db.sends("put docs {title: $1, meta: $2}", "[\"a\", {\"lang\": \"tr\", \"source\": {\"site\": \"x\", \"rank\": 3}}]")
    }

    @Test("docs: documents by paths") func documentsByPaths() async throws {
        let db = Recording()
        _ = try await db.from("docs")
            .where("meta.lang", "tr")
            .where("meta.source.rank", ">=", 2)
            .rows()
        try await db.sends("get docs where meta.lang = $1 and meta.source.rank >= $2", "[\"tr\", 2]")
    }

    @Test("docs: a path selected and ordered by") func aPathSelectedAndOrderedBy() async throws {
        let db = Recording()
        _ = try await db.from("docs")
            .select("title", "meta.source.site")
            .order("meta.source.rank", "desc")
            .rows()
        try await db.sends("get docs select title, meta.source.site order meta.source.rank desc", "[]")
    }

    @Test("docs: a path set") func aPathSet() async throws {
        let db = Recording()
        _ = try await db.from("docs")
            .where("title", "a")
            .update(["meta.lang": "en"] as Value)
        try await db.sends("set docs {meta.lang: $1} where title = $2", "[\"en\", \"a\"]")
    }

    @Test("docs: an event at a time written out") func anEventAtATimeWrittenOut() async throws {
        let db = Recording()
        _ = try await db.from("events")
            .insert(["name": "login", "t": "2026-09-19T12:34:56Z"] as Value)
        try await db.sends("put events {name: $1, t: $2}", "[\"login\", \"2026-09-19T12:34:56Z\"]")
    }

    @Test("docs: an event at epoch milliseconds") func anEventAtEpochMilliseconds() async throws {
        let db = Recording()
        _ = try await db.from("events")
            .insert(["name": "logout", "t": 1758285296000] as Value)
        try await db.sends("put events {name: $1, t: $2}", "[\"logout\", 1758285296000]")
    }

    @Test("docs: events between a day and now") func eventsBetweenADayAndNow() async throws {
        let db = Recording()
        _ = try await db.from("events")
            .where("t", ">=", "2026-01-01")
            .where(.raw("t < now()"))
            .rows()
        try await db.sends("get events where t >= $1 and t < now()", "[\"2026-01-01\"]")
    }

    @Test("docs: people in the root collation") func peopleInTheRootCollation() async throws {
        let db = Recording()
        _ = try await db.from("people")
            .order("name", "asc", collate: "und")
            .limit(20)
            .rows()
        try await db.sends("get people order name collate und asc limit $1", "[20]")
    }

    @Test("docs: people in Turkish order") func peopleInTurkishOrder() async throws {
        let db = Recording()
        _ = try await db.from("people")
            .order("city", "desc", collate: "tr")
            .order("name", "asc", collate: "tr")
            .rows()
        try await db.sends("get people order city collate tr desc, name collate tr asc", "[]")
    }

    @Test("docs: people counted by city in order") func peopleCountedByCityInOrder() async throws {
        let db = Recording()
        _ = try await db.from("people")
            .select("city", "count(*)")
            .group("city")
            .order("city", "asc", collate: "und")
            .rows()
        try await db.sends("get people select city, count(*) group city order city collate und asc", "[]")
    }

    @Test("docs: the page of people after a name") func thePageOfPeopleAfterAName() async throws {
        let db = Recording()
        _ = try await db.from("people")
            .where("name", ">", "Ada")
            .order("name")
            .limit(20)
            .rows()
        try await db.sends("get people where name > $1 order name asc limit $2", "[\"Ada\", 20]")
    }

    @Test("docs: a user") func aUser() async throws {
        let db = Recording()
        _ = try await db.from("users")
            .insert(["email": "Alice@Example.com", "name": "İsmail"] as Value)
        try await db.sends("put users {email: $1, name: $2}", "[\"Alice@Example.com\", \"İsmail\"]")
    }

    @Test("docs: a user by an email in any case") func aUserByAnEmailInAnyCase() async throws {
        let db = Recording()
        _ = try await db.from("users").where("email", "alice@example.COM").rows()
        try await db.sends("get users where email = $1", "[\"alice@example.COM\"]")
    }

    @Test("docs: a user whose email is taken") func aUserWhoseEmailIsTaken() async throws {
        let db = Recording()
        _ = try await db.from("users")
            .insert(["email": "ALICE@example.com"] as Value)
        try await db.sends("put users {email: $1}", "[\"ALICE@example.com\"]")
    }

    @Test("docs: a user unless the email is taken") func aUserUnlessTheEmailIsTaken() async throws {
        let db = Recording()
        _ = try await db.from("users")
            .insert(["email": "alice@example.com"] as Value, ifAbsent: true)
        try await db.sends("put users {email: $1} if absent", "[\"alice@example.com\"]")
    }

    @Test("docs: a user by a name in any case") func aUserByANameInAnyCase() async throws {
        let db = Recording()
        _ = try await db.from("users").where("name", "ismail").rows()
        try await db.sends("get users where name = $1", "[\"ismail\"]")
    }

    @Test("docs: users in Turkish order") func usersInTurkishOrder() async throws {
        let db = Recording()
        _ = try await db.from("users").select("name").order("name").limit(20).rows()
        try await db.sends("get users select name order name asc limit $1", "[20]")
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

    @Test("docs: a page of products with brands and colours counted") func aPageOfProductsWithBrandsAndColoursCounted() async throws {
        let db = Recording()
        _ = try await db.from("products")
            .where("price", "<", 500)
            .limit(20)
            .facet("brand")
            .facet("color")
            .rows()
        try await db.sends("get products where price < $1 limit $2 facet brand, color", "[500, 20]")
    }

    @Test("docs: a page of matches with brands and tags counted") func aPageOfMatchesWithBrandsAndTagsCounted() async throws {
        let db = Recording()
        _ = try await db.from("products")
            .where("price", "<", 500)
            .match("title", "tent")
            .limit(20)
            .facet("brand", top: 10)
            .facet("tags")
            .rows()
        try await db.sends("get products where price < $1 match title $2 limit $3 facet brand top $4, tags", "[500, \"tent\", 20, 10]")
    }

    @Test("docs: phones counted by brand") func phonesCountedByBrand() async throws {
        let db = Recording()
        _ = try await db.from("products")
            .where("category", "phones")
            .facet("brand")
            .count()
        try await db.sends("get products where category = $1 count facet brand", "[\"phones\"]")
    }

    @Test("docs: products counted by origin alone") func productsCountedByOriginAlone() async throws {
        let db = Recording()
        _ = try await db.from("products")
            .limit(0)
            .facet("meta.origin", top: 5)
            .rows()
        try await db.sends("get products limit $1 facet meta.origin top $2", "[0, 5]")
    }

    @Test("docs: red tents of two brands with their facets") func redTentsOfTwoBrandsWithTheirFacets() async throws {
        let db = Recording()
        _ = try await db.from("products")
            .where("category", "tents")
            .where("brand", "in", ["acme", "nova"])
            .where("color", "red")
            .order("price")
            .limit(24)
            .facet("brand", disjunctive: true)
            .facet("color", disjunctive: true)
            .facet("material")
            .rows()
        try await db.sends("get products where category = $1 and brand in [$2, $3] and color = $4 order price asc limit $5 facet brand disjunctive, color disjunctive, material", "[\"tents\", \"acme\", \"nova\", \"red\", 24]")
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

    @Test("docs: two jobs queued") func twoJobsQueued() async throws {
        let db = Recording()
        _ = try await db.from("jobs")
            .insert([["payload": ["to": "ada"], "run_at": .expr("now()"), "attempts": 0], ["payload": ["to": "bo"], "run_at": .expr("now()"), "attempts": 0]] as Value)
        try await db.sends("put jobs [{payload: $1, run_at: now(), attempts: $2}, {payload: $3, run_at: now(), attempts: $4}]", "[{\"to\": \"ada\"}, 0, {\"to\": \"bo\"}, 0]")
    }

    @Test("docs: the ten oldest ready jobs claimed") func theTenOldestReadyJobsClaimed() async throws {
        let db = Recording()
        _ = try await db.from("jobs")
            .where(.raw("run_at <= now()"))
            .order("run_at")
            .limit(10)
            .updateReturning(["owner": "w1", "run_at": .expr("now() + ?", 30000), "attempts": .expr("attempts + 1")] as Value)
        try await db.sends("set jobs {owner: $1, run_at: now() + $2, attempts: attempts + 1} where run_at <= now() order run_at asc limit $3 returning *", "[\"w1\", 30000, 10]")
    }

    @Test("docs: a job acknowledged") func aJobAcknowledged() async throws {
        let db = Recording()
        _ = try await db.from("jobs")
            .where("id", 1)
            .where("owner", "w1")
            .delete(require: 1)
        try await db.sends("del jobs where id = $1 and owner = $2 require 1", "[1, \"w1\"]")
    }

    @Test("docs: ready jobs popped") func readyJobsPopped() async throws {
        let db = Recording()
        _ = try await db.from("jobs")
            .where(.raw("run_at <= now()"))
            .order("run_at")
            .limit(10)
            .deleteReturning(returning: ["id", "payload.to"])
        try await db.sends("del jobs where run_at <= now() order run_at asc limit $1 returning id, payload.to", "[10]")
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

    @Test("docs: a page of documents with their comments") func aPageOfDocumentsWithTheirComments() async throws {
        let db = Recording()
        _ = try await db.from("docs")
            .where("year", ">=", 2024)
            .where(.or(["lang": "tr"], .cmp("tags", "has", "rust")))
            .order("title", "asc", collate: "tr")
            .limit(20)
            .lookup("comments", on: "doc_id", limit: 3)
            .rows()
        try await db.sends("get docs where year >= $1 and (lang = $2 or tags has $3) order title collate tr asc limit $4 lookup comments on doc_id limit $5", "[2024, \"tr\", \"rust\", 20, 3]")
    }

    @Test("docs: documents matching a misspelt word") func documentsMatchingAMisspeltWord() async throws {
        let db = Recording()
        _ = try await db.from("docs")
            .where("year", ">=", 2024)
            .match("title", "databse", fuzzy: 2)
            .limit(10)
            .rows()
        try await db.sends("get docs where year >= $1 match title $2 fuzzy 2 limit $3", "[2024, \"databse\", 10]")
    }

    @Test("docs: the document of a slug") func theDocumentOfASlug() async throws {
        let db = Recording()
        _ = try await db.from("docs").where("slug", "intro").first()
        try await db.sends("get docs where slug = $1 limit $2", "[\"intro\", 1]")
    }

    @Test("docs: documents counted since a year") func documentsCountedSinceAYear() async throws {
        let db = Recording()
        _ = try await db.from("docs").where("year", ">=", 2024).count()
        try await db.sends("get docs where year >= $1 count", "[2024]")
    }

    @Test("docs: a document inserted") func aDocumentInserted() async throws {
        let db = Recording()
        _ = try await db.from("docs")
            .insert(["slug": "intro", "title": "Ownership in Rust", "year": 2026] as Value)
        try await db.sends("put docs {slug: $1, title: $2, year: $3}", "[\"intro\", \"Ownership in Rust\", 2026]")
    }

    @Test("docs: a document inserted unless its slug is held") func aDocumentInsertedUnlessItsSlugIsHeld() async throws {
        let db = Recording()
        _ = try await db.from("docs")
            .insert(
                ["slug": "intro", "title": "Ownership in Rust"] as Value,
                ifAbsent: true
            )
        try await db.sends("put docs {slug: $1, title: $2} if absent", "[\"intro\", \"Ownership in Rust\"]")
    }

    @Test("docs: a document's views counted") func aDocumentSViewsCounted() async throws {
        let db = Recording()
        _ = try await db.from("docs")
            .where("slug", "intro")
            .update(["views": .inc(1)] as Value)
        try await db.sends("set docs {views: coalesce(views, 0) + $1} where slug = $2", "[1, \"intro\"]")
    }

    @Test("docs: the documents before a year deleted") func theDocumentsBeforeAYearDeleted() async throws {
        let db = Recording()
        _ = try await db.from("docs").where("year", "<", 2000).delete()
        try await db.sends("del docs where year < $1", "[2000]")
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
