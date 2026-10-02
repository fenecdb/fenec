package com.fenecdb

import kotlinx.coroutines.runBlocking
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNotNull
import kotlin.test.fail

/** The Kotlin tab of site/content/docs/languages.html, as it is written there, and the builder lines below it. */
class DocsTest {
    @Test
    fun theDocsExample() = runBlocking {
        val path = scratch()
        val db = Fenec.openAsync(path)

        db.execute("create collection if not exists docs (title text, embed vector<3> @hnsw(cosine))")
        db.execute("put docs {title: \$1, embed: \$2}", "Night at the oasis", floatArrayOf(0.1f, 0.2f, 0.3f))
        db.execute("put docs {title: \$1, embed: \$2}", "Dunes", floatArrayOf(0.9f, 0.1f, 0f))

        val rows = db.query("get docs select title near embed \$1 limit 5", floatArrayOf(0.1f, 0.2f, 0.3f))
        assertEquals("Night at the oasis", rows[0].string("title"))
        assertNotNull(rows[0].double("_score"))

        try {
            db.query("get nowhere")
            fail("a collection that is not there answered")
        } catch (e: FenecException) {
            assertEquals(FenecException.Code.NOT_FOUND, e.code)
        }

        val docs = db.from("docs")
        docs.insert(mapOf("title" to "Night at the oasis", "embed" to floatArrayOf(0.1f, 0.2f, 0.3f)))
        val near = docs.select("title").near("embed", floatArrayOf(0.1f, 0.2f, 0.3f)).limit(5).rows()
        val n = docs.where("title", "~", "Dunes").count()
        assertEquals(3, near.size)
        assertEquals(1, n)
        // The builder table's condition.
        assertEquals(
            "get docs where lang = \$1 or tags has \$2",
            docs.where(Cond.or(mapOf("lang" to "tr"), mapOf("tags" to mapOf("has" to "rust")))).toFenecQL().text,
        )
        db.close()
    }
}
