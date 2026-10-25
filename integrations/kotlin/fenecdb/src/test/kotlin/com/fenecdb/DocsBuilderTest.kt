// Written by site/builder_tabs.py (make docs-builder): do not edit.

// The code of each builder tab on the site's FenecQL pages, as the page
// shows it, run against a transport that records and held to the statement
// its case of integrations/builder-golden.json makes: a tab shows code this
// SDK runs. make docs-builder writes the file again as the pages' tabs
// change, and site/builder_tabs.py --check fails while it is behind them.

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

    /** docs: articles counted since a year */
    @Test
    fun articlesCountedSinceAYear() = runBlocking {
        val db = Recording()
        db.from("articles").where("year", ">=", 2024).count()
        db.sends("get articles where year >= \$1 count", "[2024]")
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
