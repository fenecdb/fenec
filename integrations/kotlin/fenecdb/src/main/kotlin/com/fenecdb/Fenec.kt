package com.fenecdb

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger

/**
 * What the library or the builder refused, and why. [code] is the kind:
 * the engine's ([Code.NOT_FOUND], [Code.DUPLICATE] ...), the boundary's
 * ([Code.MISUSE], [Code.LOCKED], [Code.PANIC]), or [Code.BUILDER] for a
 * chain the query builder refused before anything ran -- its message the JS
 * builder's, word for word.
 */
class FenecException internal constructor(
    val code: Code,
    message: String,
    /** The parameters the library asked for again as JSON (`exact`). */
    internal val exact: List<Int>? = null,
) : RuntimeException(message) {
    constructor(code: Code, message: String) : this(code, message, null)

    enum class Code(val value: Int) {
        TYPE(1), NOT_FOUND(2), EXISTS(3), DUPLICATE(4), CORRUPT(5), QUERY(6), IO(7), PLUGIN(8), READ_ONLY(9),
        DENIED(10), PANIC(11), MISUSE(12), LOCKED(13), BUILDER(100);

        companion object {
            fun of(value: Int): Code = entries.firstOrNull { it.value == value } ?: PANIC
        }
    }
}

/**
 * One value a `facet` counted and how many rows hold it. The value is any
 * JSON value the field holds -- a [String], a [Long], a [Row] for an
 * object -- and `null` counts the rows whose field is null.
 */
data class FacetCount(val value: Any?, val count: Long)

/**
 * A query's rows, and what its `facet` clause counted beside them as
 * [facets] -- empty when it asked none. A `List<Row>` itself, equal to any
 * list of the same rows, so a caller that took a list from [Query.rows] or
 * a live query takes this unchanged.
 */
class Rows(private val rows: List<Row>, val facets: Map<String, List<FacetCount>> = emptyMap()) : List<Row> by rows {
    override fun equals(other: Any?): Boolean = rows == other

    override fun hashCode(): Int = rows.hashCode()

    override fun toString(): String = rows.toString()
}

/** An answer to a statement. */
sealed class Answer {
    /**
     * Rows, and what the query's `facet` clause counted beside them: each
     * field asked, in the order asked, its values most first. Empty when
     * the query asked none.
     */
    data class Rows @JvmOverloads constructor(
        val columns: List<String>,
        override val rows: List<Row>,
        override val facets: Map<String, List<FacetCount>> = emptyMap(),
    ) : Answer()

    data class Affected(val count: Long) : Answer()

    data class Ok(val message: String) : Answer()

    data class Schemas(val collections: List<Row>) : Answer()

    /** The rows, or none. */
    open val rows: List<Row> get() = emptyList()

    /** What `facet` counted, by field; none unless the query asked. */
    open val facets: Map<String, List<FacetCount>> get() = emptyMap()

    /** The rows with the facets beside them. */
    // Named whole: inside Answer, `Rows` is the nested answer.
    val page: com.fenecdb.Rows get() = com.fenecdb.Rows(rows, facets)

    /** How many documents a write wrote, or 0. */
    val affected: Long get() = (this as? Affected)?.count ?: 0
}

/** `{"brand": [{"value": "acme", "count": 12}, ...], ...}` as the library's, the fields in the order asked. */
internal fun facetsOf(v: Row?): Map<String, List<FacetCount>> {
    if (v == null) return emptyMap()
    return v.entries.associateTo(LinkedHashMap()) { (field, counts) ->
        field to (counts as? List<*> ?: emptyList<Any?>()).map {
            val c = it as Row
            FacetCount(c["value"], c.long("count") ?: 0)
        }
    }
}

/** What [Fenec.changes] answers: the counter now, and the collections written -- `null`, everything stale. */
data class Changes(val seq: Long, val horizon: Long, val collections: List<String>?)

/**
 * A fenecdb database in a file on the device -- or in memory -- through the
 * native library (crates/fenec-ffi): the engine and the file format of the
 * server and the browser.
 *
 * ```kotlin
 * val db = Fenec.open(File(context.filesDir, "app.fenec").path)
 * db.execute("create collection todos (title text, done bool @hash)")
 * db.from("todos").insert(mapOf("title" to "milk", "done" to false))
 * val open = db.from("todos").where("done", false).rows()
 * ```
 *
 * The suspending calls run on [Dispatchers.IO]: a read waits for a write
 * under way, a write for its fsync. Java and code off the main thread call
 * the `...Blocking` ones. One instance is safe to share between threads;
 * open a file once in a process -- a second open of it is refused
 * ([FenecException.Code.LOCKED]), since two databases over one file
 * corrupt it.
 */
class Fenec private constructor(internal val handle: Long) : AutoCloseable {
    private val closed = AtomicBoolean(false)

    /** Writes under way, which a live query's look waits out. */
    private val inflight = AtomicInteger()
    internal val lives = Lives(this)

    /** The replica's sync, set once by [Fenec.Companion.sync]. */
    @Volatile internal var syncing: Replica? = null
        private set

    internal fun attach(r: Replica) {
        syncing = r
    }

    companion object {
        /** Writes wait in a buffer for [sync], [flush] or [close] rather than an fsync each. */
        const val NO_SYNC = 1

        /**
         * The file read into memory rather than mapped: for a file whose pages
         * may become unreadable while it is open, where a mapped page read
         * would be the process's end.
         */
        const val IN_MEMORY = 2

        /**
         * No compact on its own. Without it the library compacts the file
         * beside the calls once half of it is dead records -- versions
         * updates and deletes left behind -- and at least 64 MB.
         */
        const val NO_AUTO_COMPACT = 4

        /** The library's version. */
        @JvmStatic
        val version: String get() = String(FenecNative.version(), 1, FenecNative.version().size - 1, Charsets.UTF_8)

        /**
         * Opens the file at [path], made when missing. Blocking: the open reads
         * the file's index and links the vectors written since its graphs were
         * last saved, so it belongs off the main thread ([openAsync]).
         */
        @JvmStatic
        @JvmOverloads
        fun open(path: String, flags: Int = 0): Fenec {
            val digits = FenecNative.answer(FenecNative.open(path.encodeToByteArray(), flags))
            return Fenec(digits.toLong())
        }

        /** [open] on [Dispatchers.IO]. */
        suspend fun openAsync(path: String, flags: Int = 0): Fenec = withContext(Dispatchers.IO) { open(path, flags) }

        /** A database in memory alone. */
        @JvmStatic
        fun memory(): Fenec = Fenec(FenecNative.answer(FenecNative.openMemory()).toLong())
    }

    // ---------------------------------------------------------------- running

    /** Runs FenecQL -- one statement or several, which land together -- with [params] for `$1`, `$2` ... */
    suspend fun run(text: String, vararg params: Any?): Answer = runList(text, params.toList())

    suspend fun runList(text: String, params: List<Any?>): Answer = withContext(Dispatchers.IO) { answerBlocking(text, params) }

    /** The rows a statement answers. */
    suspend fun query(text: String, vararg params: Any?): List<Row> = runList(text, params.toList()).rows

    /** A write: how many documents it wrote. */
    suspend fun execute(text: String, vararg params: Any?): Long = runList(text, params.toList()).affected

    /** [run], on the calling thread. */
    fun runBlocking(text: String, vararg params: Any?): Answer = answerBlocking(text, params.toList())

    /** [query], on the calling thread. */
    fun queryBlocking(text: String, vararg params: Any?): List<Row> = answerBlocking(text, params.toList()).rows

    /** [execute], on the calling thread. */
    fun executeBlocking(text: String, vararg params: Any?): Long = answerBlocking(text, params.toList()).affected

    /** The query builder over a collection: `db.from("todos").where("done", false).rows()`. */
    fun from(collection: String): Query = Query.from(collection).bind { text, params -> runList(text, params) }

    internal fun answerBlocking(text: String, params: List<Any?>, quiet: Boolean = false): Answer {
        if (closed.get()) throw FenecException(FenecException.Code.MISUSE, "the database was closed")
        val values = params.map { Values.normalize(it) }
        if (!quiet) inflight.incrementAndGet()
        try {
            return try {
                answer(text, values, emptySet())
            } catch (e: FenecException) {
                // A json field keeps a list of numbers as written: those the
                // library asks for go again inside the JSON.
                if (e.exact == null) throw e
                answer(text, values, e.exact.toSet())
            }
        } finally {
            if (!quiet) {
                inflight.decrementAndGet()
                // After an error too: a text that failed may follow statements
                // that wrote, and the change ring says what landed.
                lives.touch()
                // A write to a synced collection left a request for the server due.
                syncing?.poll()
            }
        }
    }

    internal val writing: Boolean get() = inflight.get() > 0

    /**
     * Runs [text]: the vectors among the parameters -- a [FloatArray], or a
     * list of finite numbers, which the engine reads as a vector either way
     * -- go over as their bytes, their place `null` in the JSON, unless
     * [asJson] names them. Written out as text and read back, a 128-dim
     * vector's digits took a put from 4.6 us to 23.3 (`make ffi-bench`).
     */
    private fun answer(text: String, params: List<Any?>, asJson: Set<Int>): Answer {
        val json = params.toMutableList()
        val vectors = java.io.ByteArrayOutputStream()
        params.forEachIndexed { i, p ->
            if (i in asJson) return@forEachIndexed
            val f = Values.vector(p) ?: return@forEachIndexed
            json[i] = null
            val b = java.nio.ByteBuffer.allocate(8 + 4 * f.size).order(java.nio.ByteOrder.LITTLE_ENDIAN)
            b.putInt(i).putInt(f.size)
            // A -0 goes as 0, as JSON writes it: either way, one vector.
            for (x in f) b.putFloat(if (x == 0f) 0f else x)
            vectors.write(b.array())
        }
        val out = FenecNative.answer(
            FenecNative.query(
                handle,
                text.encodeToByteArray(),
                Json.write(json).encodeToByteArray(),
                if (vectors.size() == 0) null else vectors.toByteArray(),
            ),
        )
        return decode(Json.parse(out) as Row)
    }

    private fun decode(v: Row): Answer = when (v.string("kind")) {
        "rows" -> {
            val r = v.row("result")
            Answer.Rows(
                r?.list("columns")?.map { it as String } ?: emptyList(),
                r?.list("rows")?.map { it as Row } ?: emptyList(),
                facetsOf(r?.row("facets")),
            )
        }
        "affected" -> Answer.Affected(v.long("count") ?: 0)
        "ok" -> Answer.Ok(v.string("message") ?: "")
        "schemas" -> Answer.Schemas(v.list("collections")?.map { it as Row } ?: emptyList())
        else -> throw FenecException(FenecException.Code.QUERY, "an answer of no kind the binding knows: $v")
    }

    // ---------------------------------------------------------------- changes

    /** What changed since [since]: the counter now, and the collections written. */
    fun changes(since: Long): Changes {
        val v = Json.parse(FenecNative.answer(FenecNative.changes(handle, since))) as Row
        return Changes(
            v.long("seq") ?: 0,
            v.long("horizon") ?: 0,
            v.list("collections")?.map { it as String },
        )
    }

    // ------------------------------------------------------------- durability

    /** Every write so far on disk, written and fsynced, the fsync with no lock held. */
    suspend fun sync() = withContext(Dispatchers.IO) { syncBlocking() }

    fun syncBlocking() {
        FenecNative.answer(FenecNative.sync(handle))
    }

    /**
     * Every write so far handed to the system, no fsync: it outlives the app
     * being killed, not the device losing power -- microseconds, what an app
     * does in `onStop`.
     */
    suspend fun flush() = withContext(Dispatchers.IO) { flushBlocking() }

    fun flushBlocking() {
        FenecNative.answer(FenecNative.flush(handle))
    }

    /** The file written anew as an image of the database, graphs and all: the next open links nothing. */
    suspend fun checkpoint() = withContext(Dispatchers.IO) { checkpointBlocking() }

    fun checkpointBlocking() {
        FenecNative.answer(FenecNative.checkpoint(handle))
    }

    /** Saves the graphs, syncs and lets the file go; the live queries stop. Waits for the calls under way. */
    override fun close() {
        if (!closed.compareAndSet(false, true)) return
        syncing?.stop()
        lives.clear()
        FenecNative.answer(FenecNative.close(handle))
    }
}

/** What a parameter or a document's field may be, and how it goes. */
internal object Values {
    /**
     * A value as it goes into the parameters: a [java.util.Date] or an
     * `Instant` as the text JavaScript's `toISOString` writes, which the JS
     * builder sends a `Date` as, inside lists and maps too.
     */
    fun normalize(v: Any?): Any? = when (v) {
        null, is String, is Boolean, is Number, is FloatArray, is DoubleArray, is IntArray, is LongArray -> v
        is java.util.Date -> Json.iso(v.time)
        is Map<*, *> -> LinkedHashMap<String, Any?>().also { m -> v.forEach { (k, x) -> m[k.toString()] = normalize(x) } }
        is List<*> -> v.map { normalize(it) }
        is Array<*> -> v.map { normalize(it) }
        is CharSequence -> v.toString()
        else -> instant(v) ?: throw FenecException(
            FenecException.Code.BUILDER,
            "this object cannot be used as a fenecdb value: ${v.javaClass.name}",
        )
    }

    /** `java.time.Instant`, which Android has from API 26: by name, so older ones load this class. */
    private fun instant(v: Any): String? {
        if (v.javaClass.name != "java.time.Instant") return null
        return Json.iso(v.javaClass.getMethod("toEpochMilli").invoke(v) as Long)
    }

    /** A vector: a [FloatArray], or a list of finite numbers. */
    fun vector(v: Any?): FloatArray? {
        when (v) {
            is FloatArray -> return v.takeIf { it.isNotEmpty() }
            is DoubleArray -> return if (v.isNotEmpty() && v.all { it.isFinite() }) FloatArray(v.size) { v[it].toFloat() } else null
            is List<*> -> {
                if (v.isEmpty()) return null
                val out = FloatArray(v.size)
                for ((i, x) in v.withIndex()) {
                    val d = (x as? Number)?.toDouble() ?: return null
                    if (!d.isFinite()) return null
                    out[i] = d.toFloat()
                }
                return out
            }
            else -> return null
        }
    }
}
