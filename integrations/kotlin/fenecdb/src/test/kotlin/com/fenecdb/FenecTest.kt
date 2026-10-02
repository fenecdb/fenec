package com.fenecdb

import kotlinx.coroutines.async
import kotlinx.coroutines.awaitAll
import kotlinx.coroutines.runBlocking
import java.io.File
import java.nio.file.Files
import kotlin.math.sqrt
import kotlin.test.Test
import kotlin.test.assertContentEquals
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertNull
import kotlin.test.assertTrue

/** A file of its own in a directory of its own, as an app's files directory holds one. */
fun scratch(): String = File(Files.createTempDirectory("fenecdb-kotlin").toFile(), "app.fenec").path

class FenecTest {
    @Test
    fun aStatementAndItsRows() = runBlocking {
        val db = Fenec.memory()
        assertEquals(Answer.Ok("collection `notes` created"), db.run("create collection notes (title text, stars int, embed vector<3> @hnsw(cosine))"))
        val n = db.execute(
            "put notes [{title: \$1, stars: 5, embed: \$2}, {title: \$3, stars: 3, embed: \$4}]",
            "oasis", floatArrayOf(1f, 0f, 0f), "dunes", floatArrayOf(0f, 1f, 0f),
        )
        assertEquals(2, n)
        val rows = db.query("get notes select title, stars order stars desc")
        assertEquals(listOf("oasis", "dunes"), rows.map { it.string("title") })
        assertEquals(5L, rows[0]["stars"])
        assertEquals(listOf("title", "stars"), rows[0].keys.toList())
        db.close()
    }

    /** `near` scores as the engine computes them, to the Float: the vector goes over as its bytes. */
    @Test
    fun nearScoresAreExact() = runBlocking {
        val db = Fenec.memory()
        db.execute("create collection v (n int, e vector<2> @hnsw(l2))")
        db.execute("put v [{n: 1, e: [1, 0]}, {n: 2, e: [0, 1]}, {n: 3, e: [0.5, 0.25]}]")
        val rows = db.query("get v select n near e \$1 limit 3", floatArrayOf(1f, 0f))
        assertEquals(listOf(1L, 3L, 2L), rows.map { it.long("n") })
        val third = sqrt((0.5f - 1f) * (0.5f - 1f) + 0.25f * 0.25f)
        assertEquals(listOf(0f, third, sqrt(2f)), rows.map { it.double("_score")!!.toFloat() })
        // A list of numbers is a vector too, as a page's are.
        assertEquals(2L, db.query("get v select n near e \$1 limit 1", listOf(0.0, 1.0)).first().long("n"))
        assertContentEquals(floatArrayOf(0.5f, 0.25f), db.query("get v select e where n = 3").first().floats("e"))
        db.close()
    }

    @Test
    fun theBuilderWritesAndReads() = runBlocking {
        val db = Fenec.memory()
        db.execute("create collection notes (title text, stars int, at timestamp)")
        val notes = db.from("notes")
        val at = java.util.Date(1_789_821_296_789L)
        assertEquals(2, notes.insert(listOf(mapOf("title" to "a", "stars" to 4, "at" to at), mapOf("title" to "b", "stars" to 2))))
        assertEquals("put notes {title: \$1, stars: \$2}", notes.toInsert(mapOf("title" to "a", "stars" to 4)).text)
        assertEquals("2026-09-19T12:34:56.789Z", notes.toInsert(mapOf("at" to at)).params[0])
        assertEquals(listOf("a", "b"), notes.order("stars", "desc").rows().map { it.string("title") })
        assertEquals(4L, notes.where("title", "a").first()?.long("stars"))
        assertNull(notes.where("title", "zz").first())
        assertEquals(2, notes.count())
        assertEquals(1, notes.where("stars", ">", 3).update(mapOf("stars" to 5)))
        assertEquals(1, notes.where("stars", 5).delete())
        assertEquals(1, notes.countBlocking())
        db.close()
    }

    /** A json field keeps a list of numbers as written: sent as Floats, it is sent again as JSON. */
    @Test
    fun aJsonFieldTakesItsListAsWritten() = runBlocking {
        val db = Fenec.memory()
        db.execute("create collection t (meta json)")
        db.execute("put t {meta: \$1}", listOf(0.1, 0.2))
        assertEquals(listOf(0.1, 0.2), db.query("get t select meta").first()["meta"])
        db.close()
    }

    @Test
    fun errorsCarryTheirKind() = runBlocking {
        val db = Fenec.memory()
        db.execute("create collection t (a int @unique)")
        db.execute("put t {a: 1}")
        for ((text, code) in listOf(
            "get nowhere" to FenecException.Code.NOT_FOUND,
            "broken query" to FenecException.Code.QUERY,
            "create collection t (a int)" to FenecException.Code.EXISTS,
            "put t {a: 1}" to FenecException.Code.DUPLICATE,
            "put t {a: \"x\"}" to FenecException.Code.TYPE,
        )) {
            val e = assertFailsWith<FenecException>(text) { db.run(text) }
            assertEquals(code, e.code, text)
        }
        val e = assertFailsWith<FenecException> { db.from("t; drop collection t") }
        assertEquals(FenecException.Code.BUILDER, e.code)
        assertEquals("invalid collection name: \"t; drop collection t\"", e.message)
        assertFailsWith<FenecException> { db.from("t").where("a", 1).insert(mapOf("a" to 2)) }
        db.close()
        assertEquals("the database was closed", assertFailsWith<FenecException> { db.run("get t") }.message)
        db.close()
    }

    @Test
    fun aFileIsThereAgainAndOpenOnce() = runBlocking {
        val path = scratch()
        var db = Fenec.openAsync(path)
        db.execute("create collection t (title text, e vector<2> @hnsw(cosine))")
        for (i in 0 until 20) db.execute("put t {title: \$1, e: \$2}", "n$i", floatArrayOf(i / 10f, 1f))
        assertEquals(FenecException.Code.LOCKED, assertFailsWith<FenecException> { Fenec.open(path) }.code)
        db.checkpoint()
        db.close()
        db = Fenec.open(path, Fenec.NO_SYNC)
        assertEquals(20, db.from("t").count())
        db.execute("put t {title: \"after\"}")
        db.flush()
        db.sync()
        db.close()
        db = Fenec.open(path, Fenec.IN_MEMORY)
        assertEquals(21, db.from("t").count())
        assertEquals("n0", db.from("t").select("title").near("e", floatArrayOf(0f, 1f)).limit(1).rows().first().string("title"))
        db.close()
    }

    @Test
    fun manyCoroutinesShareOneDatabase() = runBlocking {
        val db = Fenec.open(scratch())
        db.execute("create collection t (w int, e vector<2> @hnsw(l2))")
        (0 until 8).map { w ->
            async(kotlinx.coroutines.Dispatchers.IO) {
                for (i in 0 until 20) {
                    if (w % 2 == 0) {
                        db.execute("put t {w: \$1, e: \$2}", w, floatArrayOf(w.toFloat(), i.toFloat()))
                    } else {
                        db.query("get t near e \$1 limit 3", floatArrayOf(0f, 1f))
                    }
                }
            }
        }.awaitAll()
        assertEquals(80, db.from("t").count())
        db.close()
    }

    @Test
    fun javaCallsBlock() {
        val db = Fenec.memory()
        db.executeBlocking("create collection t (a int)")
        assertEquals(1, db.executeBlocking("put t {a: \$1}", 7))
        assertEquals(7L, db.queryBlocking("get t").first().long("a"))
        assertEquals(1, db.from("t").rowsBlocking().size)
        db.close()
    }

    @Test
    fun valuesWriteAndReadAsJson() {
        val v = linkedMapOf("s" to "a\"b\n\u0001ç😀", "n" to -1L, "f" to 0.5, "ok" to true, "none" to null, "list" to listOf(1L, 2.5))
        val text = Json.write(v)
        assertEquals("{\"s\":\"a\\\"b\\n\\u0001ç😀\",\"n\":-1,\"f\":0.5,\"ok\":true,\"none\":null,\"list\":[1,2.5]}", text)
        assertEquals(v, Json.parse(text))
        assertEquals("😀", (Json.parse("{\"e\":\"\\ud83d\\ude00\"}") as Row).string("e"))
        val tie = 7.038531e-26f
        assertEquals(tie, Json.number(tie).toDouble().toFloat())
        assertEquals("0.1", Json.number(0.1f))
        assertEquals("1969-12-31T23:59:59.999Z", Json.iso(-1))
        assertTrue(Fenec.version.split('.').size == 3)
    }
}
