// Written by site/builder_tabs.py (make docs-builder): do not edit.

// The code of each builder tab on the site, as the page shows it, run
// against a transport that records and held to the statement its case of
// integrations/builder-golden.json makes: a tab shows code this SDK runs.
// make docs-builder writes the file again as the pages' tabs change, and
// site/builder_tabs.py --check fails while it is behind them.

package com.fenecdb

import kotlinx.coroutines.runBlocking
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

class DocsBuilderTest {
    /** What the tabs call `db`: queries bound to a recorder, as a database's are to its engine. */
    private class Recording {
        var sent: Statement? = null

        fun from(collection: String): Query = Query.from(collection).bind { text, params ->
            sent = Statement(text, params)
            when {
                Regex("^(put|set|del) ").containsMatchIn(text) -> Answer.Affected(0)
                text.endsWith(" count") -> Answer.Rows(listOf("count"), listOf(Row(mapOf("count" to 0L))))
                else -> Answer.Rows(emptyList(), emptyList())
            }
        }

        fun sends(text: String, params: String) {
            assertEquals(text, sent?.text)
            val got = Json.parse(Json.write(sent?.params))
            assertTrue(GoldenTest.same(got, Json.parse(params)), "params $got, want $params")
        }
    }

    /** docs: aggregates over every order */
    @Test
    fun aggregatesOverEveryOrder() = runBlocking {
        val db = Recording()
        db.from("orders")
            .select("count(*)", "sum(total)", "avg(total)", "min(at)", "max(at)")
            .rows()
        db.sends("get orders select count(*), sum(total), avg(total), min(at), max(at)", "[]")
    }

    /** docs: aggregates of each status */
    @Test
    fun aggregatesOfEachStatus() = runBlocking {
        val db = Recording()
        db.from("orders")
            .select("status", "count(*)", "avg(total)")
            .where("year", 2026)
            .group("status")
            .rows()
        db.sends("get orders select status, count(*), avg(total) where year = \$1 group status", "[2026]")
    }

    /** docs: the ten customers who spent most */
    @Test
    fun theTenCustomersWhoSpentMost() = runBlocking {
        val db = Recording()
        db.from("orders")
            .select("customer", "sum(total)")
            .group("customer")
            .order("sum(total)", "desc")
            .limit(10)
            .rows()
        db.sends("get orders select customer, sum(total) group customer order sum(total) desc limit \$1", "[10]")
    }

    /** docs: distinct users an hour and event */
    @Test
    fun distinctUsersAnHourAndEvent() = runBlocking {
        val db = Recording()
        db.from("events")
            .select(
                Computed.bucket("at", "1h").alias("hour"),
                "name",
                Computed.countDistinct("user").alias("users")
            )
            .group("hour", "name")
            .rows()
        db.sends("get events select bucket(at, 1h) as hour, name, count(distinct user) as users group hour, name", "[]")
    }

    /** docs: one-minute bars of a symbol */
    @Test
    fun oneMinuteBarsOfASymbol() = runBlocking {
        val db = Recording()
        db.from("ticks")
            .select(
                Computed.bucket("at", "1m").alias("bar"),
                Computed.first("px", "at").alias("open"),
                Computed.expr("max(px)").alias("high"),
                Computed.expr("min(px)").alias("low"),
                Computed.last("px", "at").alias("close"),
                Computed.expr("sum(qty)").alias("volume")
            )
            .where("sym", "S07")
            .group("bar")
            .rows()
        db.sends("get ticks select bucket(at, 1m) as bar, first(px by at) as open, max(px) as high, min(px) as low, last(px by at) as close, sum(qty) as volume where sym = \$1 group bar", "[\"S07\"]")
    }

    /** docs: vwap of each symbol */
    @Test
    fun vwapOfEachSymbol() = runBlocking {
        val db = Recording()
        db.from("ticks")
            .select("sym", Computed.expr("sum(px * qty) / sum(qty)").alias("vwap"))
            .group("sym")
            .rows()
        db.sends("get ticks select sym, sum(px * qty) / sum(qty) as vwap group sym", "[]")
    }

    /** docs: totals of a json path */
    @Test
    fun totalsOfAJsonPath() = runBlocking {
        val db = Recording()
        db.from("events")
            .select("props.plan", "sum(props.v)")
            .group("props.plan")
            .rows()
        db.sends("get events select props.plan, sum(props.v) group props.plan", "[]")
    }

    /** docs: the three statuses that took most */
    @Test
    fun theThreeStatusesThatTookMost() = runBlocking {
        val db = Recording()
        db.from("orders")
            .select("status", "count(*)", "sum(total)")
            .where("year", 2026)
            .group("status")
            .order("sum(total)", "desc")
            .limit(3)
            .rows()
        db.sends("get orders select status, count(*), sum(total) where year = \$1 group status order sum(total) desc limit \$2", "[2026, 3]")
    }

    /** docs: one-minute bars since a time */
    @Test
    fun oneMinuteBarsSinceATime() = runBlocking {
        val db = Recording()
        db.from("ticks")
            .select(
                Computed.bucket("at", "1m").alias("bar"),
                Computed.first("px", "at").alias("open"),
                Computed.expr("max(px)").alias("high"),
                Computed.expr("min(px)").alias("low"),
                Computed.last("px", "at").alias("close"),
                Computed.expr("sum(qty)").alias("volume")
            )
            .where("sym", "S07")
            .where("at", ">=", "2026-05-03T09:00:00Z")
            .group("bar")
            .rows()
        db.sends("get ticks select bucket(at, 1m) as bar, first(px by at) as open, max(px) as high, min(px) as low, last(px by at) as close, sum(qty) as volume where sym = \$1 and at >= \$2 group bar", "[\"S07\", \"2026-05-03T09:00:00Z\"]")
    }

    /** docs: the newest articles */
    @Test
    fun theNewestArticles() = runBlocking {
        val db = Recording()
        db.from("articles").order("published", "desc").limit(20).rows()
        db.sends("get articles order published desc limit \$1", "[20]")
    }

    /** docs: the newest articles since a day */
    @Test
    fun theNewestArticlesSinceADay() = runBlocking {
        val db = Recording()
        db.from("articles")
            .where("published", ">=", "2026-09-01")
            .order("published", "desc")
            .limit(20)
            .rows()
        db.sends("get articles where published >= \$1 order published desc limit \$2", "[\"2026-09-01\", 20]")
    }

    /** docs: articles counted over a range of titles */
    @Test
    fun articlesCountedOverARangeOfTitles() = runBlocking {
        val db = Recording()
        db.from("articles")
            .where("title", ">=", "k")
            .where("title", "<", "l")
            .count()
        db.sends("get articles where title >= \$1 and title < \$2 count", "[\"k\", \"l\"]")
    }

    /** docs: the nearest articles since a day */
    @Test
    fun theNearestArticlesSinceADay() = runBlocking {
        val db = Recording()
        db.from("articles")
            .where("published", ">=", "2026-01-01")
            .near("embed", listOf(0.9, 0.1, 0.0))
            .limit(10)
            .rows()
        db.sends("get articles where published >= \$1 near embed \$2 limit \$3", "[\"2026-01-01\", [0.9, 0.1, 0], 10]")
    }

    /** docs: an owner's newest posts */
    @Test
    fun anOwnerSNewestPosts() = runBlocking {
        val db = Recording()
        db.from("posts")
            .where("owner", 7)
            .order("created", "desc")
            .limit(20)
            .rows()
        db.sends("get posts where owner = \$1 order created desc limit \$2", "[7, 20]")
    }

    /** docs: three owners' newest posts */
    @Test
    fun threeOwnersNewestPosts() = runBlocking {
        val db = Recording()
        db.from("posts")
            .where("owner", "in", listOf(7, 8, 9))
            .order("created", "desc")
            .limit(20)
            .rows()
        db.sends("get posts where owner in [\$1, \$2, \$3] order created desc limit \$4", "[7, 8, 9, 20]")
    }

    /** docs: an owner's posts counted since a time */
    @Test
    fun anOwnerSPostsCountedSinceATime() = runBlocking {
        val db = Recording()
        db.from("posts")
            .where("owner", 7)
            .where("created", ">=", "2026-10-01")
            .count()
        db.sends("get posts where owner = \$1 and created >= \$2 count", "[7, \"2026-10-01\"]")
    }

    /** docs: a session seen */
    @Test
    fun aSessionSeen() = runBlocking {
        val db = Recording()
        db.from("sessions")
            .insert(mapOf("user" to "ada", "seen" to Computed.expr("now()")))
        db.sends("put sessions {user: \$1, seen: now()}", "[\"ada\"]")
    }

    /** docs: a session seen again */
    @Test
    fun aSessionSeenAgain() = runBlocking {
        val db = Recording()
        db.from("sessions")
            .where("user", "ada")
            .update(mapOf("seen" to Computed.expr("now()")))
        db.sends("set sessions {seen: now()} where user = \$1", "[\"ada\"]")
    }

    /** docs: a user's sessions */
    @Test
    fun aUserSSessions() = runBlocking {
        val db = Recording()
        db.from("sessions").where("user", "ada").rows()
        db.sends("get sessions where user = \$1", "[\"ada\"]")
    }

    /** docs: products with their newest reviews */
    @Test
    fun productsWithTheirNewestReviews() = runBlocking {
        val db = Recording()
        db.from("products")
            .limit(20)
            .lookup(
                "reviews",
                on = "product_id",
                order = listOf(SortKey("created", "desc")),
                limit = 3
            )
            .rows()
        db.sends("get products limit \$1 lookup reviews on product_id order created desc limit \$2", "[20, 3]")
    }

    /** docs: products with their reviews */
    @Test
    fun productsWithTheirReviews() = runBlocking {
        val db = Recording()
        db.from("products").lookup("reviews", on = "product_id").rows()
        db.sends("get products lookup reviews on product_id", "[]")
    }

    /** docs: the names of dear products with their good reviews */
    @Test
    fun theNamesOfDearProductsWithTheirGoodReviews() = runBlocking {
        val db = Recording()
        db.from("products")
            .select("name")
            .where("price", ">=", 10000)
            .lookup(
                "reviews",
                on = "product_id",
                select = listOf("body"),
                where = Cond.cmp("stars", "gte", 4),
                limit = 3
            )
            .rows()
        db.sends("get products select name where price >= \$1 lookup reviews on product_id select body where stars >= \$2 limit \$3", "[10000, 4, 3]")
    }

    /** docs: products with their tags by sku */
    @Test
    fun productsWithTheirTagsBySku() = runBlocking {
        val db = Recording()
        db.from("products").lookup("tags", on = "sku", parentKey = "sku").rows()
        db.sends("get products lookup tags on sku = sku", "[]")
    }

    /** docs: shops, their orders and the orders' lines */
    @Test
    fun shopsTheirOrdersAndTheOrdersLines() = runBlocking {
        val db = Recording()
        db.from("shops")
            .limit(20)
            .lookup("orders", on = "shop_id", limit = 3)
            .lookup("lines", on = "order_id", limit = 5)
            .rows()
        db.sends("get shops limit \$1 lookup orders on shop_id limit \$2 lookup lines on order_id limit \$3", "[20, 3, 5]")
    }

    /** docs: orders with their lines by a key of two fields */
    @Test
    fun ordersWithTheirLinesByAKeyOfTwoFields() = runBlocking {
        val db = Recording()
        db.from("orders")
            .where("org", 7)
            .limit(20)
            .lookup(
                "lines",
                on = listOf("org", "order"),
                parentKey = listOf("org", "id"),
                order = listOf(SortKey("line")),
                limit = 5
            )
            .rows()
        db.sends("get orders where org = \$1 limit \$2 lookup lines on (org, order) = (org, id) order line asc limit \$3", "[7, 20, 5]")
    }

    /** docs: shops with their orders folded */
    @Test
    fun shopsWithTheirOrdersFolded() = runBlocking {
        val db = Recording()
        db.from("shops")
            .limit(20)
            .lookup(
                "orders",
                on = "shop",
                select = listOf(Computed.expr("count(*)").alias("n"), Computed.expr("sum(total)").alias("revenue"))
            )
            .rows()
        db.sends("get shops limit \$1 lookup orders on shop select count(*) as n, sum(total) as revenue", "[20]")
    }

    /** docs: couriers with their latest fifty deliveries folded */
    @Test
    fun couriersWithTheirLatestFiftyDeliveriesFolded() = runBlocking {
        val db = Recording()
        db.from("couriers")
            .limit(20)
            .lookup(
                "deliveries",
                on = "courier",
                select = listOf(Computed.expr("count(*)").alias("n"), Computed.expr("avg(km)").alias("km")),
                order = listOf(SortKey("at", "desc")),
                limit = 50
            )
            .rows()
        db.sends("get couriers limit \$1 lookup deliveries on courier select count(*) as n, avg(km) as km order at desc limit \$2", "[20, 50]")
    }

    /** docs: each shop's three best sellers */
    @Test
    fun eachShopSThreeBestSellers() = runBlocking {
        val db = Recording()
        db.from("shops")
            .lookup(
                "lines",
                on = "shop",
                select = listOf("product", Computed.expr("sum(qty)").alias("sold")),
                group = listOf("product"),
                order = listOf(SortKey("sold", "desc")),
                limit = 3
            )
            .rows()
        db.sends("get shops lookup lines on shop select product, sum(qty) as sold group product order sold desc limit \$1", "[3]")
    }

    /** docs: the busiest couriers with their names */
    @Test
    fun theBusiestCouriersWithTheirNames() = runBlocking {
        val db = Recording()
        db.from("orders")
            .select("courier", Computed.expr("count(*)").alias("n"))
            .where("at", ">=", "2026-10-01")
            .group("courier")
            .order("n", "desc")
            .limit(10)
            .lookup(
                "couriers",
                on = "user",
                parentKey = "courier",
                select = listOf("name")
            )
            .rows()
        db.sends("get orders select courier, count(*) as n where at >= \$1 group courier order n desc limit \$2 lookup couriers on user = courier select name", "[\"2026-10-01\", 10]")
    }

    /** docs: products with a five-star review */
    @Test
    fun productsWithAFiveStarReview() = runBlocking {
        val db = Recording()
        db.from("products")
            .limit(20)
            .lookup(
                "reviews",
                on = "product_id",
                required = true,
                where = mapOf("stars" to 5)
            )
            .rows()
        db.sends("get products limit \$1 lookup reviews on product_id required where stars = \$2", "[20, 5]")
    }

    /** docs: products with a five-star review counted */
    @Test
    fun productsWithAFiveStarReviewCounted() = runBlocking {
        val db = Recording()
        db.from("products")
            .lookup(
                "reviews",
                on = "product_id",
                required = true,
                where = mapOf("stars" to 5)
            )
            .count()
        db.sends("get products count lookup reviews on product_id required where stars = \$1", "[5]")
    }

    /** docs: articles a page at a time */
    @Test
    fun articlesAPageAtATime() = runBlocking {
        val db = Recording()
        db.from("articles")
            .where("year", 2024)
            .order("published", "desc")
            .limit(20)
            .offset(40)
            .rows()
        db.sends("get articles where year = \$1 order published desc limit \$2 offset \$3", "[2024, 20, 40]")
    }

    /** docs: a page of matches with its brands counted */
    @Test
    fun aPageOfMatchesWithItsBrandsCounted() = runBlocking {
        val db = Recording()
        db.from("products")
            .match("title", "tent")
            .limit(20)
            .facet("brand", top = 10)
            .rows()
        db.sends("get products match title \$1 limit \$2 facet brand top \$3", "[\"tent\", 20, 10]")
    }

    /** docs: ready jobs claimed a page at a time */
    @Test
    fun readyJobsClaimedAPageAtATime() = runBlocking {
        val db = Recording()
        db.from("jobs")
            .where(Cond.raw("run_at <= now()"))
            .order("run_at")
            .limit(10)
            .updateReturning(mapOf("owner" to "w1"))
        db.sends("set jobs {owner: \$1} where run_at <= now() order run_at asc limit \$2 returning *", "[\"w1\", 10]")
    }

    /** docs: a column worked out */
    @Test
    fun aColumnWorkedOut() = runBlocking {
        val db = Recording()
        db.from("ticks")
            .select("sym", Computed.expr("px * qty").alias("notional"))
            .where("qty", ">", 100)
            .rows()
        db.sends("get ticks select sym, px * qty as notional where qty > \$1", "[100]")
    }

    /** docs: columns named and worked out */
    @Test
    fun columnsNamedAndWorkedOut() = runBlocking {
        val db = Recording()
        db.from("ticks")
            .select(
                Computed.expr("px").alias("price"),
                Computed.bucket("at", "1m").alias("minute"),
                Computed.expr("greatest(px, ?)", 100).alias("floor")
            )
            .rows()
        db.sends("get ticks select px as price, bucket(at, 1m) as minute, greatest(px, \$1) as floor", "[100]")
    }

    /** docs: a column of a case */
    @Test
    fun aColumnOfACase() = runBlocking {
        val db = Recording()
        db.from("orders")
            .select(
                "id",
                Computed.expr("case when total >= ? then ? else ? end", 100, "large", "small").alias("size")
            )
            .rows()
        db.sends("get orders select id, case when total >= \$1 then \$2 else \$3 end as size", "[100, \"large\", \"small\"]")
    }

    /** docs: articles counted since a year */
    @Test
    fun articlesCountedSinceAYear() = runBlocking {
        val db = Recording()
        db.from("articles").where("year", ">=", 2024).count()
        db.sends("get articles where year >= \$1 count", "[2024]")
    }

    /** docs: a document with a json field */
    @Test
    fun aDocumentWithAJsonField() = runBlocking {
        val db = Recording()
        db.from("docs")
            .insert(mapOf("title" to "a", "meta" to mapOf("lang" to "tr", "source" to mapOf("site" to "x", "rank" to 3))))
        db.sends("put docs {title: \$1, meta: \$2}", "[\"a\", {\"lang\": \"tr\", \"source\": {\"site\": \"x\", \"rank\": 3}}]")
    }

    /** docs: documents by paths */
    @Test
    fun documentsByPaths() = runBlocking {
        val db = Recording()
        db.from("docs")
            .where("meta.lang", "tr")
            .where("meta.source.rank", ">=", 2)
            .rows()
        db.sends("get docs where meta.lang = \$1 and meta.source.rank >= \$2", "[\"tr\", 2]")
    }

    /** docs: a path selected and ordered by */
    @Test
    fun aPathSelectedAndOrderedBy() = runBlocking {
        val db = Recording()
        db.from("docs")
            .select("title", "meta.source.site")
            .order("meta.source.rank", "desc")
            .rows()
        db.sends("get docs select title, meta.source.site order meta.source.rank desc", "[]")
    }

    /** docs: a path set */
    @Test
    fun aPathSet() = runBlocking {
        val db = Recording()
        db.from("docs").where("title", "a").update(mapOf("meta.lang" to "en"))
        db.sends("set docs {meta.lang: \$1} where title = \$2", "[\"en\", \"a\"]")
    }

    /** docs: an event at a time written out */
    @Test
    fun anEventAtATimeWrittenOut() = runBlocking {
        val db = Recording()
        db.from("events")
            .insert(mapOf("name" to "login", "t" to "2026-09-19T12:34:56Z"))
        db.sends("put events {name: \$1, t: \$2}", "[\"login\", \"2026-09-19T12:34:56Z\"]")
    }

    /** docs: an event at epoch milliseconds */
    @Test
    fun anEventAtEpochMilliseconds() = runBlocking {
        val db = Recording()
        db.from("events")
            .insert(mapOf("name" to "logout", "t" to 1758285296000))
        db.sends("put events {name: \$1, t: \$2}", "[\"logout\", 1758285296000]")
    }

    /** docs: events between a day and now */
    @Test
    fun eventsBetweenADayAndNow() = runBlocking {
        val db = Recording()
        db.from("events")
            .where("t", ">=", "2026-01-01")
            .where(Cond.raw("t < now()"))
            .rows()
        db.sends("get events where t >= \$1 and t < now()", "[\"2026-01-01\"]")
    }

    /** docs: people in the root collation */
    @Test
    fun peopleInTheRootCollation() = runBlocking {
        val db = Recording()
        db.from("people").order("name", "asc", collate = "und").limit(20).rows()
        db.sends("get people order name collate und asc limit \$1", "[20]")
    }

    /** docs: people in Turkish order */
    @Test
    fun peopleInTurkishOrder() = runBlocking {
        val db = Recording()
        db.from("people")
            .order("city", "desc", collate = "tr")
            .order("name", "asc", collate = "tr")
            .rows()
        db.sends("get people order city collate tr desc, name collate tr asc", "[]")
    }

    /** docs: people counted by city in order */
    @Test
    fun peopleCountedByCityInOrder() = runBlocking {
        val db = Recording()
        db.from("people")
            .select("city", "count(*)")
            .group("city")
            .order("city", "asc", collate = "und")
            .rows()
        db.sends("get people select city, count(*) group city order city collate und asc", "[]")
    }

    /** docs: the page of people after a name */
    @Test
    fun thePageOfPeopleAfterAName() = runBlocking {
        val db = Recording()
        db.from("people")
            .where("name", ">", "Ada")
            .order("name")
            .limit(20)
            .rows()
        db.sends("get people where name > \$1 order name asc limit \$2", "[\"Ada\", 20]")
    }

    /** docs: a user */
    @Test
    fun aUser() = runBlocking {
        val db = Recording()
        db.from("users")
            .insert(mapOf("email" to "Alice@Example.com", "name" to "İsmail"))
        db.sends("put users {email: \$1, name: \$2}", "[\"Alice@Example.com\", \"İsmail\"]")
    }

    /** docs: a user by an email in any case */
    @Test
    fun aUserByAnEmailInAnyCase() = runBlocking {
        val db = Recording()
        db.from("users").where("email", "alice@example.COM").rows()
        db.sends("get users where email = \$1", "[\"alice@example.COM\"]")
    }

    /** docs: a user whose email is taken */
    @Test
    fun aUserWhoseEmailIsTaken() = runBlocking {
        val db = Recording()
        db.from("users").insert(mapOf("email" to "ALICE@example.com"))
        db.sends("put users {email: \$1}", "[\"ALICE@example.com\"]")
    }

    /** docs: a user unless the email is taken */
    @Test
    fun aUserUnlessTheEmailIsTaken() = runBlocking {
        val db = Recording()
        db.from("users")
            .insert(mapOf("email" to "alice@example.com"), ifAbsent = true)
        db.sends("put users {email: \$1} if absent", "[\"alice@example.com\"]")
    }

    /** docs: a user by a name in any case */
    @Test
    fun aUserByANameInAnyCase() = runBlocking {
        val db = Recording()
        db.from("users").where("name", "ismail").rows()
        db.sends("get users where name = \$1", "[\"ismail\"]")
    }

    /** docs: users in Turkish order */
    @Test
    fun usersInTurkishOrder() = runBlocking {
        val db = Recording()
        db.from("users").select("name").order("name").limit(20).rows()
        db.sends("get users select name order name asc limit \$1", "[20]")
    }

    /** docs: articles matching words */
    @Test
    fun articlesMatchingWords() = runBlocking {
        val db = Recording()
        db.from("articles").match("body", "ownership").limit(10).rows()
        db.sends("get articles match body \$1 limit \$2", "[\"ownership\", 10]")
    }

    /** docs: articles matching words since a year */
    @Test
    fun articlesMatchingWordsSinceAYear() = runBlocking {
        val db = Recording()
        db.from("articles")
            .where("year", ">=", 2024)
            .match("body", "ownership")
            .limit(10)
            .rows()
        db.sends("get articles where year >= \$1 match body \$2 limit \$3", "[2024, \"ownership\", 10]")
    }

    /** docs: articles matching words reranked */
    @Test
    fun articlesMatchingWordsReranked() = runBlocking {
        val db = Recording()
        db.from("articles")
            .match("body", "ownership")
            .rerank("embed", listOf(0.9, 0.1, 0.0), candidates = 500)
            .limit(10)
            .rows()
        db.sends("get articles match body \$1 rerank embed \$2 candidates 500 limit \$3", "[\"ownership\", [0.9, 0.1, 0], 10]")
    }

    /** docs: matches with their marks */
    @Test
    fun matchesWithTheirMarks() = runBlocking {
        val db = Recording()
        db.from("articles")
            .select("title")
            .highlight("body")
            .match("body", "ownership")
            .limit(10)
            .rows()
        db.sends("get articles select title, highlight(body) match body \$1 limit \$2", "[\"ownership\", 10]")
    }

    /** docs: matches with their marks tagged */
    @Test
    fun matchesWithTheirMarksTagged() = runBlocking {
        val db = Recording()
        db.from("articles")
            .select("title")
            .highlight("body", pre = "<mark>", post = "</mark>")
            .match("body", "ownership")
            .limit(10)
            .rows()
        db.sends("get articles select title, highlight(body, \$1, \$2) match body \$3 limit \$4", "[\"<mark>\", \"</mark>\", \"ownership\", 10]")
    }

    /** docs: matches with a snippet */
    @Test
    fun matchesWithASnippet() = runBlocking {
        val db = Recording()
        db.from("articles")
            .select("title")
            .snippet("body", 20, ellipsis = "…")
            .match("body", "ownership")
            .limit(10)
            .rows()
        db.sends("get articles select title, snippet(body, 20, \$1) match body \$2 limit \$3", "[\"…\", \"ownership\", 10]")
    }

    /** docs: matches with a snippet tagged */
    @Test
    fun matchesWithASnippetTagged() = runBlocking {
        val db = Recording()
        db.from("articles")
            .select("title")
            .snippet("body", 20, ellipsis = "…", pre = "<b>", post = "</b>")
            .match("body", "ownership")
            .limit(10)
            .rows()
        db.sends("get articles select title, snippet(body, 20, \$1, \$2, \$3) match body \$4 limit \$5", "[\"…\", \"<b>\", \"</b>\", \"ownership\", 10]")
    }

    /** docs: a page of products with brands and colours counted */
    @Test
    fun aPageOfProductsWithBrandsAndColoursCounted() = runBlocking {
        val db = Recording()
        db.from("products")
            .where("price", "<", 500)
            .limit(20)
            .facet("brand")
            .facet("color")
            .rows()
        db.sends("get products where price < \$1 limit \$2 facet brand, color", "[500, 20]")
    }

    /** docs: a page of matches with brands and tags counted */
    @Test
    fun aPageOfMatchesWithBrandsAndTagsCounted() = runBlocking {
        val db = Recording()
        db.from("products")
            .where("price", "<", 500)
            .match("title", "tent")
            .limit(20)
            .facet("brand", top = 10)
            .facet("tags")
            .rows()
        db.sends("get products where price < \$1 match title \$2 limit \$3 facet brand top \$4, tags", "[500, \"tent\", 20, 10]")
    }

    /** docs: phones counted by brand */
    @Test
    fun phonesCountedByBrand() = runBlocking {
        val db = Recording()
        db.from("products").where("category", "phones").facet("brand").count()
        db.sends("get products where category = \$1 count facet brand", "[\"phones\"]")
    }

    /** docs: products counted by origin alone */
    @Test
    fun productsCountedByOriginAlone() = runBlocking {
        val db = Recording()
        db.from("products").limit(0).facet("meta.origin", top = 5).rows()
        db.sends("get products limit \$1 facet meta.origin top \$2", "[0, 5]")
    }

    /** docs: red tents of two brands with their facets */
    @Test
    fun redTentsOfTwoBrandsWithTheirFacets() = runBlocking {
        val db = Recording()
        db.from("products")
            .where("category", "tents")
            .where("brand", "in", listOf("acme", "nova"))
            .where("color", "red")
            .order("price")
            .limit(24)
            .facet("brand", disjunctive = true)
            .facet("color", disjunctive = true)
            .facet("material")
            .rows()
        db.sends("get products where category = \$1 and brand in [\$2, \$3] and color = \$4 order price asc limit \$5 facet brand disjunctive, color disjunctive, material", "[\"tents\", \"acme\", \"nova\", \"red\", 24]")
    }

    /** docs: tents counted by price range */
    @Test
    fun tentsCountedByPriceRange() = runBlocking {
        val db = Recording()
        db.from("products")
            .where("category", "tents")
            .limit(24)
            .facet(
                "price",
                ranges = listOf(0, 2500, 5000, 10000, 25000),
                disjunctive = true
            )
            .rows()
        db.sends("get products where category = \$1 limit \$2 facet price ranges [0, 2500, 5000, 10000, 25000] disjunctive", "[\"tents\", 24]")
    }

    /** docs: the nearest articles */
    @Test
    fun theNearestArticles() = runBlocking {
        val db = Recording()
        db.from("articles")
            .near("embed", listOf(0.9, 0.1, 0.0))
            .limit(10)
            .rows()
        db.sends("get articles near embed \$1 limit \$2", "[[0.9, 0.1, 0], 10]")
    }

    /** docs: the nearest articles since a year */
    @Test
    fun theNearestArticlesSinceAYear() = runBlocking {
        val db = Recording()
        db.from("articles")
            .where("year", ">=", 2024)
            .near("embed", listOf(0.9, 0.1, 0.0), ef = 200)
            .limit(10)
            .rows()
        db.sends("get articles where year >= \$1 near embed \$2 ef 200 limit \$3", "[2024, [0.9, 0.1, 0], 10]")
    }

    /** docs: the nearest articles exactly */
    @Test
    fun theNearestArticlesExactly() = runBlocking {
        val db = Recording()
        db.from("articles")
            .near("embed", listOf(0.9, 0.1, 0.0), exact = true)
            .limit(10)
            .rows()
        db.sends("get articles near embed \$1 exact limit \$2", "[[0.9, 0.1, 0], 10]")
    }

    /** docs: the nearest articles filtered */
    @Test
    fun theNearestArticlesFiltered() = runBlocking {
        val db = Recording()
        db.from("articles")
            .select("title", "year")
            .where("year", ">=", 2024)
            .where("tags", "has", "rust")
            .where(Cond.not(Cond.cmp("title", "~", "draft")))
            .near("embed", listOf(0.9, 0.1, 0.0), ef = 128)
            .limit(10)
            .rows()
        db.sends("get articles select title, year where year >= \$1 and tags has \$2 and not (title ~ \$3) near embed \$4 ef 128 limit \$5", "[2024, \"rust\", \"draft\", [0.9, 0.1, 0], 10]")
    }

    /** docs: a debit that must find the money, or says why */
    @Test
    fun aDebitThatMustFindTheMoneyOrSaysWhy() = runBlocking {
        val db = Recording()
        db.from("accounts")
            .where("name", "ada")
            .where("balance", ">=", 30)
            .update(
                mapOf("balance" to Computed.expr("balance - ?", 30)),
                require = 1,
                reason = "insufficient funds"
            )
        db.sends("set accounts {balance: balance - \$1} where name = \$2 and balance >= \$3 require 1 else 'insufficient funds'", "[30, \"ada\", 30]")
    }

    /** docs: a hold deleted that must be there, or says why */
    @Test
    fun aHoldDeletedThatMustBeThereOrSaysWhy() = runBlocking {
        val db = Recording()
        db.from("holds")
            .where("order_ref", "o-17")
            .where("state", "held")
            .delete(require = 1, reason = "it's not held")
        db.sends("del holds where order_ref = \$1 and state = \$2 require 1 else 'it''s not held'", "[\"o-17\", \"held\"]")
    }

    /** docs: a coupon that must still be usable, or says why */
    @Test
    fun aCouponThatMustStillBeUsableOrSaysWhy() = runBlocking {
        val db = Recording()
        db.from("coupons")
            .where("code", "X1")
            .where(Cond.raw("uses < max_uses"))
            .limit(1)
            .require(1, reason = "coupon used up")
            .rows()
        db.sends("get coupons where code = \$1 and uses < max_uses limit \$2 require 1 else 'coupon used up'", "[\"X1\", 1]")
    }

    /** docs: a product that must be there at its price */
    @Test
    fun aProductThatMustBeThereAtItsPrice() = runBlocking {
        val db = Recording()
        db.from("products")
            .where("sku", "tee")
            .where("price", 1200)
            .where("stock", ">", 0)
            .require(1)
            .rows()
        db.sends("get products where sku = \$1 and price = \$2 and stock > \$3 require 1", "[\"tee\", 1200, 0]")
    }

    /** docs: a coupon that must still be usable */
    @Test
    fun aCouponThatMustStillBeUsable() = runBlocking {
        val db = Recording()
        db.from("coupons")
            .where("code", "X1")
            .where(Cond.raw("uses < max_uses"))
            .limit(1)
            .require(1)
            .rows()
        db.sends("get coupons where code = \$1 and uses < max_uses limit \$2 require 1", "[\"X1\", 1]")
    }

    /** docs: no hold that must be there */
    @Test
    fun noHoldThatMustBeThere() = runBlocking {
        val db = Recording()
        db.from("holds")
            .where("order_ref", "o-17")
            .where("state", "held")
            .require(0)
            .rows()
        db.sends("get holds where order_ref = \$1 and state = \$2 require 0", "[\"o-17\", \"held\"]")
    }

    /** docs: two jobs queued */
    @Test
    fun twoJobsQueued() = runBlocking {
        val db = Recording()
        db.from("jobs")
            .insert(listOf(mapOf("payload" to mapOf("to" to "ada"), "run_at" to Computed.expr("now()"), "attempts" to 0), mapOf("payload" to mapOf("to" to "bo"), "run_at" to Computed.expr("now()"), "attempts" to 0)))
        db.sends("put jobs [{payload: \$1, run_at: now(), attempts: \$2}, {payload: \$3, run_at: now(), attempts: \$4}]", "[{\"to\": \"ada\"}, 0, {\"to\": \"bo\"}, 0]")
    }

    /** docs: the ten oldest ready jobs claimed */
    @Test
    fun theTenOldestReadyJobsClaimed() = runBlocking {
        val db = Recording()
        db.from("jobs")
            .where(Cond.raw("run_at <= now()"))
            .order("run_at")
            .limit(10)
            .updateReturning(mapOf("owner" to "w1", "run_at" to Computed.expr("now() + ?", 30000), "attempts" to Computed.expr("attempts + 1")))
        db.sends("set jobs {owner: \$1, run_at: now() + \$2, attempts: attempts + 1} where run_at <= now() order run_at asc limit \$3 returning *", "[\"w1\", 30000, 10]")
    }

    /** docs: a job acknowledged */
    @Test
    fun aJobAcknowledged() = runBlocking {
        val db = Recording()
        db.from("jobs").where("id", 1).where("owner", "w1").delete(require = 1)
        db.sends("del jobs where id = \$1 and owner = \$2 require 1", "[1, \"w1\"]")
    }

    /** docs: ready jobs popped */
    @Test
    fun readyJobsPopped() = runBlocking {
        val db = Recording()
        db.from("jobs")
            .where(Cond.raw("run_at <= now()"))
            .order("run_at")
            .limit(10)
            .deleteReturning(returning = listOf("id", "payload.to"))
        db.sends("del jobs where run_at <= now() order run_at asc limit \$1 returning id, payload.to", "[10]")
    }

    /** docs: an article set by id */
    @Test
    fun anArticleSetById() = runBlocking {
        val db = Recording()
        db.from("articles").where("id", 42).update(mapOf("year" to 2025))
        db.sends("set articles {year: \$1} where id = \$2", "[2025, 42]")
    }

    /** docs: an article's views counted */
    @Test
    fun anArticleSViewsCounted() = runBlocking {
        val db = Recording()
        db.from("articles")
            .where("id", 42)
            .update(mapOf("views" to Computed.inc(1)))
        db.sends("set articles {views: coalesce(views, 0) + \$1} where id = \$2", "[1, 42]")
    }

    /** docs: the articles before a year deleted */
    @Test
    fun theArticlesBeforeAYearDeleted() = runBlocking {
        val db = Recording()
        db.from("articles").where("year", "<", 2000).delete()
        db.sends("del articles where year < \$1", "[2000]")
    }

    /** docs: a page of documents with their comments */
    @Test
    fun aPageOfDocumentsWithTheirComments() = runBlocking {
        val db = Recording()
        db.from("docs")
            .where("year", ">=", 2024)
            .where(Cond.or(mapOf("lang" to "tr"), Cond.cmp("tags", "has", "rust")))
            .order("title", "asc", collate = "tr")
            .limit(20)
            .lookup("comments", on = "doc_id", limit = 3)
            .rows()
        db.sends("get docs where year >= \$1 and (lang = \$2 or tags has \$3) order title collate tr asc limit \$4 lookup comments on doc_id limit \$5", "[2024, \"tr\", \"rust\", 20, 3]")
    }

    /** docs: documents matching a misspelt word */
    @Test
    fun documentsMatchingAMisspeltWord() = runBlocking {
        val db = Recording()
        db.from("docs")
            .where("year", ">=", 2024)
            .match("title", "databse", fuzzy = 2)
            .limit(10)
            .rows()
        db.sends("get docs where year >= \$1 match title \$2 fuzzy 2 limit \$3", "[2024, \"databse\", 10]")
    }

    /** docs: the document of a slug */
    @Test
    fun theDocumentOfASlug() = runBlocking {
        val db = Recording()
        db.from("docs").where("slug", "intro").first()
        db.sends("get docs where slug = \$1 limit \$2", "[\"intro\", 1]")
    }

    /** docs: documents counted since a year */
    @Test
    fun documentsCountedSinceAYear() = runBlocking {
        val db = Recording()
        db.from("docs").where("year", ">=", 2024).count()
        db.sends("get docs where year >= \$1 count", "[2024]")
    }

    /** docs: a document inserted */
    @Test
    fun aDocumentInserted() = runBlocking {
        val db = Recording()
        db.from("docs")
            .insert(mapOf("slug" to "intro", "title" to "Ownership in Rust", "year" to 2026))
        db.sends("put docs {slug: \$1, title: \$2, year: \$3}", "[\"intro\", \"Ownership in Rust\", 2026]")
    }

    /** docs: a document inserted unless its slug is held */
    @Test
    fun aDocumentInsertedUnlessItsSlugIsHeld() = runBlocking {
        val db = Recording()
        db.from("docs")
            .insert(
                mapOf("slug" to "intro", "title" to "Ownership in Rust"),
                ifAbsent = true
            )
        db.sends("put docs {slug: \$1, title: \$2} if absent", "[\"intro\", \"Ownership in Rust\"]")
    }

    /** docs: a document's views counted */
    @Test
    fun aDocumentSViewsCounted() = runBlocking {
        val db = Recording()
        db.from("docs")
            .where("slug", "intro")
            .update(mapOf("views" to Computed.inc(1)))
        db.sends("set docs {views: coalesce(views, 0) + \$1} where slug = \$2", "[1, \"intro\"]")
    }

    /** docs: the documents before a year deleted */
    @Test
    fun theDocumentsBeforeAYearDeleted() = runBlocking {
        val db = Recording()
        db.from("docs").where("year", "<", 2000).delete()
        db.sends("del docs where year < \$1", "[2000]")
    }

    /** escapes: every character a literal escapes, in a filter */
    @Test
    fun escapesEveryCharacterALiteralEscapesInAFilter() = runBlocking {
        val db = Recording()
        db.from("docs")
            .where(
                "title",
                "\u0000\u0001\u0009\u000a\u000d\u001f\u0085\u2028\u2029 \"'\\\$`\${x}\$1\\(x) …"
            )
            .rows()
        db.sends("get docs where title = \$1", "[\"\\u0000\\u0001\\t\\n\\r\\u001f\u0085\u2028\u2029 \\\"'\\\\\$`\${x}\$1\\\\(x) …\"]")
    }

    /** escapes: line ends, in a document */
    @Test
    fun escapesLineEndsInADocument() = runBlocking {
        val db = Recording()
        db.from("docs")
            .where("id", 1)
            .update(mapOf("title" to "one\u2028two\u000d\u000athree\u0085four\u2029", "note" to "\u0000\u0001\u0009\u000a\u000d\u001f\u0085\u2028\u2029 \"'\\\$`\${x}\$1\\(x) …"))
        db.sends("set docs {title: \$1, note: \$2} where id = \$3", "[\"one\u2028two\\r\\nthree\u0085four\u2029\", \"\\u0000\\u0001\\t\\n\\r\\u001f\u0085\u2028\u2029 \\\"'\\\\\$`\${x}\$1\\\\(x) …\", 1]")
    }
}
