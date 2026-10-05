package com.fenecdb

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import java.util.concurrent.atomic.AtomicReference

/**
 * A fenec-server over HTTP, with no file on the device: every query goes to
 * the server (`POST /query`), through the same builder. What
 * [Fenec.Companion.connect] makes, as `connect` does in JS.
 *
 * ```kotlin
 * val db = Fenec.connect("https://api.example.com", token = jwt)
 * val open = db.from("todos").where("done", false).rows()
 * ```
 *
 * The requests are `HttpURLConnection`'s, so TLS is the system's.
 */
class FenecRemote private constructor(
    url: String,
    /** The token and the last `Fenec-Seq`, shared with every copy [withIdempotencyKey] makes, as Go's and .NET's copies share theirs. */
    private val shared: Shared,
    private val idempotencyKey: String?,
) {
    internal constructor(url: String, token: String?) : this(url, Shared(token), null)

    private class Shared(@Volatile var token: String?) {
        val seq = AtomicReference<Long?>(null)
    }

    val url: String = url.trimEnd('/')

    /** A fresh token for the requests from here on. */
    fun setToken(token: String?) {
        shared.token = token
    }

    /** The change the last write through this connection, or a copy of it, left the database at (`Fenec-Seq`); null before any. */
    val seq: Long? get() = shared.seq.get()

    /**
     * A copy whose writes -- [run], [batch] and the builder's -- carry [key]
     * as their `Idempotency-Key`: sent again after a timeout, a write is
     * answered as it was the first time and not made twice. One key a write:
     * the same key with another request is refused (status 422).
     *
     * ```kotlin
     * db.withIdempotencyKey(orderId).from("orders").insert(order)
     * ```
     */
    fun withIdempotencyKey(key: String): FenecRemote {
        require(key.isNotEmpty()) { "an idempotency key is a text, not empty" }
        return FenecRemote(url, shared, key)
    }

    /** Runs FenecQL on the server with [params] for `$1`, `$2` ... */
    suspend fun run(text: String, vararg params: Any?): Answer = runList(text, params.toList())

    /** [run], its parameters as a list. With [idempotencyKey] a write runs once however often it is sent. */
    suspend fun runList(text: String, params: List<Any?>, idempotencyKey: String? = null): Answer =
        withContext(Dispatchers.IO) { send(text, params, idempotencyKey) }

    /** The rows a statement answers. */
    suspend fun query(text: String, vararg params: Any?): List<Row> = runList(text, params.toList()).rows

    /** A write: how many documents it wrote. */
    suspend fun execute(text: String, vararg params: Any?): Long = runList(text, params.toList()).affected

    /** The query builder, its queries run on the server. */
    fun from(collection: String): Query = Query.from(collection).bind { text, params -> runList(text, params) }

    /** [run], on the calling thread. */
    fun runBlocking(text: String, vararg params: Any?): Answer = send(text, params.toList(), null)

    /**
     * What [batch] answers: each statement's answer in order, the change the
     * batch left the database at, and whether the answer is the one kept for
     * its idempotency key ([replayed]) -- a replayed answer carries no [seq].
     */
    data class BatchAnswer(val results: List<Answer>, val seq: Long?, val replayed: Boolean)

    /**
     * `POST /batch`: the statements in order under one write lock, as one
     * block -- their writes all land, or at the first error none of them do,
     * a [FenecException] whose [FenecException.at] is the statement that
     * stopped it ([FenecException.Code.UNMET] for a write's `require` not
     * met). A statement is what the builder's `toInsert`, `toUpdate`,
     * `toDelete` and `toFenecQL` make. With [idempotencyKey] a retry after a
     * timeout is answered as the first try was and writes nothing twice; a
     * batch of reads alone takes no key.
     */
    suspend fun batch(statements: List<Statement>, idempotencyKey: String? = null): BatchAnswer =
        withContext(Dispatchers.IO) {
            val lines = statements.joinToString("\n") { s ->
                Json.write(mapOf("query" to s.text, "params" to s.params.map { Values.normalize(it) }))
            }
            val (v, seq, replayed) = post("/batch", "application/x-ndjson", lines, idempotencyKey)
            val results = ((v as? Row)?.list("results") ?: emptyList()).map { answer(it ?: Row(emptyMap())) }
            BatchAnswer(results, seq, replayed)
        }

    private fun send(text: String, params: List<Any?>, key: String?): Answer {
        val body = Json.write(mapOf("query" to text, "params" to params.map { Values.normalize(it) }))
        return answer(post("/query", "application/json", body, key).first)
    }

    /**
     * A POST's JSON answer, its `Fenec-Seq` and whether it was replayed; a
     * refusal as a [FenecException] with its status and, for a batch, where
     * it stopped.
     */
    private fun post(path: String, type: String, payload: String, key: String?): Triple<Any, Long?, Boolean> {
        val c = URL("$url$path").openConnection() as HttpURLConnection
        c.requestMethod = "POST"
        c.connectTimeout = 10_000
        c.readTimeout = 60_000
        c.setRequestProperty("content-type", type)
        shared.token?.let { c.setRequestProperty("authorization", "Bearer $it") }
        (key ?: idempotencyKey)?.let { c.setRequestProperty("idempotency-key", it) }
        c.doOutput = true
        val (status, body) = try {
            c.outputStream.use { it.write(payload.encodeToByteArray()) }
            val status = c.responseCode
            status to ((if (status >= 400) c.errorStream else c.inputStream)?.use { String(it.readBytes(), Charsets.UTF_8) } ?: "")
        } catch (e: IOException) {
            throw FenecException(FenecException.Code.IO, "the server could not be reached: ${e.message}")
        }
        val v = runCatching { if (body.isEmpty()) null else Json.parse(body) }.getOrNull()
        if (status !in 200..299) {
            val r = v as? Row
            throw FenecException(
                code(status), r?.string("error") ?: "HTTP $status", null,
                status = status, at = r?.int("at"), completed = r?.int("completed"),
            )
        }
        v ?: throw FenecException(FenecException.Code.IO, "the server did not answer JSON ($status)")
        val seq = c.getHeaderField("fenec-seq")?.toLongOrNull()
        if (seq != null) shared.seq.set(seq)
        return Triple(v, seq, c.getHeaderField("idempotent-replayed") == "true")
    }

    internal companion object {
        /** An HTTP status as the error kind the engine would have said; the status itself is the error's `status`. */
        fun code(status: Int) = when (status) {
            400 -> FenecException.Code.QUERY
            401, 403 -> FenecException.Code.DENIED
            404 -> FenecException.Code.NOT_FOUND
            409 -> FenecException.Code.DUPLICATE
            412 -> FenecException.Code.UNMET
            else -> FenecException.Code.IO
        }

        /**
         * The endpoint's answer as the library's: rows come as an array, or
         * -- when the query asked for facets, and as a batch's each read --
         * as `{"rows": [...], "facets": {...}}`.
         */
        fun answer(v: Any): Answer = when {
            v is List<*> -> rowsOf(v, null)
            v is Row && v.list("rows") != null -> rowsOf(v.list("rows")!!, v.row("facets"))
            v is Row && v.long("affected") != null -> Answer.Affected(v.long("affected")!!)
            v is Row && v.list("collections") != null -> Answer.Schemas(v.list("collections")!!.map { it as Row })
            v is Row -> Answer.Ok(v.string("message") ?: Json.write(v))
            else -> Answer.Ok(Json.write(v))
        }

        private fun rowsOf(list: List<*>, facets: Row?): Answer.Rows {
            val rows = list.map { it as Row }
            return Answer.Rows(rows.firstOrNull()?.keys?.toList() ?: emptyList(), rows, facetsOf(facets))
        }
    }
}

/** A fenec-server over HTTP, with no local file: every query goes to the server, through the same builder. */
fun Fenec.Companion.connect(url: String, token: String? = null): FenecRemote = FenecRemote(url, token)
