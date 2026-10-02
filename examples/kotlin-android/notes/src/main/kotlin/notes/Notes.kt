package notes

import com.fenecdb.Fenec
import com.fenecdb.Query
import com.fenecdb.schema
import java.util.Date
import kotlin.math.sqrt

/** The Notes app's database: its schema, its seed notes and its queries. */
object Notes {
    /** create collection notes (...): applied after every open, which makes what is missing. */
    val schema: String = Notes::class.java.getResource("/schema.fenecql")!!.readText()

    private val columns = listOf("id", "title", "body", "tags", "done", "at")

    /** Opens (or makes) the file, applies the schema, and seeds an empty collection. */
    suspend fun open(path: String): Fenec {
        val db = Fenec.openAsync(path)
        db.schema(schema)
        if (db.from("notes").count() == 0L) seed(db)
        return db
    }

    suspend fun seed(db: Fenec) {
        add(db, "Groceries", "Buy milk, eggs and fresh bread for the weekend.", listOf("home", "shopping"), at = "2026-09-28T09:00:00Z")
        add(db, "Release checklist", "Tag the release, publish the packages and update the docs.", listOf("work"), at = "2026-09-29T09:00:00Z")
        add(db, "Book flights", "Find cheap flights to Istanbul for the conference in spring.", listOf("travel", "work"), done = true, at = "2026-09-30T09:00:00Z")
        add(db, "Book club", "Finish the novel about the desert fox before Thursday.", listOf("home", "reading"), at = "2026-10-01T09:00:00Z")
    }

    /** A new note; `at` is now unless given (a Date goes in as ISO-8601). */
    suspend fun add(db: Fenec, title: String, body: String, tags: List<String>, done: Boolean = false, at: Any = Date()) {
        db.from("notes").insert(
            mapOf("title" to title, "body" to body, "tags" to tags, "done" to done, "at" to at, "embed" to embed("$title $body")),
        )
    }

    suspend fun done(db: Fenec, id: Long) {
        db.from("notes").where("id", id).update(mapOf("done" to true))
    }

    fun list(db: Fenec): Query = db.from("notes").select(columns).order("at", "desc").limit(20)

    fun tagged(db: Fenec, tag: String): Query = list(db).where("tags", "has", tag)

    fun pending(db: Fenec): Query = list(db).where("done", false)

    /** BM25 over the body's words. */
    fun search(db: Fenec, words: String): Query = db.from("notes").select(columns).match("body", words).limit(20)

    /** The nearest notes by the toy embedding. */
    fun similar(db: Fenec, words: String): Query = db.from("notes").select(columns).near("embed", embed(words)).limit(20)

    /** The words and the vector, their rankings fused. */
    fun hybrid(db: Fenec, words: String): Query =
        db.from("notes").select(columns).match("body", words).near("embed", embed(words)).fuse().limit(20)

    /**
     * A TOY embedding, a placeholder for a real model: hashed character
     * trigrams (of the UTF-8 bytes) into 64 dimensions, the same in every
     * example. A real app calls an embeddings API here, or runs a model on
     * the device (ONNX Runtime, TensorFlow Lite), and declares its size in
     * schema.fenecql's `vector<N>`.
     */
    fun embed(text: String): FloatArray {
        val lowered = buildString { for (c in text) append(if (c in 'A'..'Z') c + 32 else c) }
        val bytes = " $lowered ".encodeToByteArray()
        val v = FloatArray(64)
        for (i in 0..bytes.size - 3) {
            var h = 0x811c9dc5.toInt()
            for (j in i until i + 3) h = (h xor (bytes[j].toInt() and 0xff)) * 0x01000193
            v[(h.toUInt() % 64u).toInt()] += 1f
        }
        val norm = sqrt(v.sumOf { it.toDouble() * it })
        return if (norm > 0) FloatArray(64) { (v[it] / norm).toFloat() } else v
    }
}
