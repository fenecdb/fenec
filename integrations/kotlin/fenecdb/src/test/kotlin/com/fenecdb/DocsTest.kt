package com.fenecdb

import kotlinx.coroutines.runBlocking
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue
import kotlin.test.fail

/** The Kotlin tab of site/content/docs/languages.html, as it is written there, and the builder lines below it. */
class DocsTest {
    @Test
    fun theDocsExample() = runBlocking {
        val path = scratch()
        val db = Fenec.openAsync(path)

        db.execute("create collection if not exists docs (title text @text, year int @sorted)")
        db.execute("put docs {title: \$1, year: \$2}", "Dunes", 2021)
        db.execute("put docs {title: \$1, year: \$2}", "Night at the oasis", 2024)

        val rows = db.query("get docs select title, year where year >= \$1 order year desc limit \$2", 2020, 5)
        assertEquals(listOf("Night at the oasis", "Dunes"), rows.map { it.string("title") })
        assertEquals(2024, rows[0].int("year"))
        val hits = db.query("get docs select title match title \$1 fuzzy", "oasys")
        assertEquals(listOf("Night at the oasis"), hits.map { it.string("title") })
        assertTrue(hits[0].double("_score")!! > 0)

        try {
            db.query("get nowhere")
            fail("a collection that is not there answered")
        } catch (e: FenecException) {
            assertEquals(FenecException.Code.NOT_FOUND, e.code)
        }

        val docs = db.from("docs")
        docs.insert(mapOf("title" to "Night at the oasis", "year" to 2024))
        val recent = docs.select("title", "year").where("year", ">=", 2020).order("year", "desc").limit(5).rows()
        val found = docs.select("title").match("title", "oasys", fuzzy = 2).rows()
        val n = docs.where("title", "~", "Dunes").count()
        assertEquals(listOf("Night at the oasis", "Night at the oasis", "Dunes"), recent.map { it.string("title") })
        assertEquals(2, found.size)
        assertEquals(1, n)
        // The builder table's condition.
        assertEquals(
            "get docs where lang = \$1 or tags has \$2",
            docs.where(Cond.or(mapOf("lang" to "tr"), mapOf("tags" to mapOf("has" to "rust")))).toFenecQL().text,
        )
        db.close()
    }
}
