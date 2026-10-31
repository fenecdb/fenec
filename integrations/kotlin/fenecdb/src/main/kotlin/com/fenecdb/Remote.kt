package com.fenecdb

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.asExecutor
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withContext
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL

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
    /**
     * The token, the last `Fenec-Seq` and the furthest position, shared with
     * every copy [withIdempotencyKey], [withWait] and [asRead] make, as Go's
     * and .NET's copies share theirs.
     */
    private val shared: Shared,
    private val idempotencyKey: String?,
    /** How long the server may hold a write that writes nothing, ms ([withWait]). */
    private val held: Long? = null,
    /** This copy's requests only read ([asRead], the builder's reads). */
    private val reading: Boolean = false,
) {
    internal constructor(url: String, token: String?, reads: Reads = Reads.HOME) :
        this(url, Shared(token, reads), null)

    /**
     * Where a connection's reads go. [HOME], the default: every request to
     * the tenant's home. [NEAREST]: the builder's reads, and an [asRead]
     * copy's requests, are marked (`Fenec-Read: replica`) so that a
     * fenec-shard router near a region holding the tenant's read replica
     * answers them there, each sent after the furthest position an answer
     * from home named to this connection (`Fenec-After`, `Fenec-History`:
     * the change and its history) -- never older than its own writes, nor a
     * write a failover replaced. Writes go home.
     */
    enum class Reads { HOME, NEAREST }

    private class Shared(@Volatile var token: String?, val reads: Reads) {
        /** The furthest position an answer from home named, which a read near is sent after: an answer that arrives late is not the last. */
        private var at: Position? = null

        @Synchronized fun position(): Position? = at

        /** An answer from home named [p]. */
        @Synchronized fun answered(p: Position) {
            if (p.isPast(at)) at = p
        }

        /**
         * Home's position, lower or not, which a read near's 504 names when
         * no copy held the one it was sent after, [sent] -- a write lost in a
         * failover: kept, every read near would wait it out for good. Taken
         * only over the position sent -- a write answered meanwhile is
         * further, and set back the next read near would miss it -- and
         * never of an earlier term of the same lineage: a router whose copy
         * of the directory lags a failover sends the read to the old
         * primary, fenced, which names the branch it abandoned. Another
         * lineage's is a tenant made again under the name, which starts
         * again at term 1.
         */
        @Synchronized fun told(sent: Position, p: Position) {
            if (at == sent && (p.term >= sent.term || p.isOfAnotherLineage(sent))) at = p
        }
    }

    val url: String = url.trimEnd('/')

    /** Where this connection's reads go ([Fenec.Companion.connect]'s `reads`). */
    val reads: Reads get() = shared.reads

    /** A fresh token for the requests from here on. */
    fun setToken(token: String?) {
        shared.token = token
    }

    /**
     * The furthest change an answer from home -- a write's, a read-only
     * batch's -- left the database at (`Fenec-Seq`), through this
     * connection or a copy of it; null before any. Answers arriving out of
     * order do not move it back, a read near's answer, which may be a
     * replica's, moves it not, and a read near's 504 sets it to where home
     * says it stands, as Go's `Seq()` and .NET's `Seq` are.
     */
    val seq: Long? get() = shared.position()?.seq

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
        return FenecRemote(url, shared, key, held, reading)
    }

    /**
     * A copy whose `set` and `del` statements -- [run], [batch] and the
     * builder's `update`, `delete` and their `Returning` forms -- the server
     * holds while they write nothing, until they can or for [millis], at
     * most 30 000 (`Fenec-Wait`): a worker's claim waits there for a job --
     * one enqueued, a delayed one come due, a lease lapsed -- rather than
     * sleeping and claiming again. Cancelling the coroutine closes the
     * connection, which gives the claim up at once.
     *
     * ```kotlin
     * val claimed = db.withWait(30_000).from("jobs").where(Cond.raw("run_at <= now()"))
     *     .order("run_at").limit(10).updateReturning(claim)
     * ```
     */
    fun withWait(millis: Long): FenecRemote {
        require(millis >= 0) { "a wait is milliseconds, from 0" }
        return FenecRemote(url, shared, idempotencyKey, millis, reading)
    }

    /**
     * A copy whose requests only read: on a connection made with `reads =
     * Reads.NEAREST` a router near the tenant's read replica answers them
     * there, after this connection's writes. The builder's reads go so on
     * their own; a text the caller knows reads -- a [run], a [batch] of
     * `toFenecQL()`s -- goes through this, and one that writes is refused
     * (status 403), never landed. On a connection reading home it changes
     * nothing.
     *
     * ```kotlin
     * val n = db.asRead().query("get orders where customer = $1 count", "c-7")
     * ```
     */
    fun asRead(): FenecRemote = FenecRemote(url, shared, idempotencyKey, held, true)

    /** Runs FenecQL on the server with [params] for `$1`, `$2` ... */
    suspend fun run(text: String, vararg params: Any?): Answer = runList(text, params.toList())

    /** [run], its parameters as a list. With [idempotencyKey] a write runs once however often it is sent. */
    suspend fun runList(text: String, params: List<Any?>, idempotencyKey: String? = null): Answer {
        val body = Json.write(mapOf("query" to text, "params" to params.map { Values.normalize(it) }))
        return answer(posting("/query", "application/json", body, idempotencyKey).first)
    }

    /** The rows a statement answers. */
    suspend fun query(text: String, vararg params: Any?): List<Row> = runList(text, params.toList()).rows

    /** A write: how many documents it wrote. */
    suspend fun execute(text: String, vararg params: Any?): Long = runList(text, params.toList()).affected

    /**
     * The query builder, its queries run on the server -- its reads as an
     * [asRead] copy's, near on a connection reading near.
     */
    fun from(collection: String): Query {
        val reader = asRead()
        return Query.from(collection).bind(
            { text, params -> runList(text, params) },
            { text, params -> reader.runList(text, params) },
        )
    }

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
     * met, its [FenecException.reason] the one the statement gave, which
     * stays put as statements go in front of it where `at` moves). A
     * statement is what the builder's `toInsert`, `toUpdate`,
     * `toDelete` and `toFenecQL` make. With [idempotencyKey] a retry after a
     * timeout is answered as the first try was and writes nothing twice; a
     * batch of reads alone takes no key.
     */
    suspend fun batch(statements: List<Statement>, idempotencyKey: String? = null): BatchAnswer {
        val lines = statements.joinToString("\n") { s ->
            Json.write(mapOf("query" to s.text, "params" to s.params.map { Values.normalize(it) }))
        }
        val (v, seq, replayed) = posting("/batch", "application/x-ndjson", lines, idempotencyKey)
        val results = ((v as? Row)?.list("results") ?: emptyList()).map { answer(it ?: Row(emptyMap())) }
        return BatchAnswer(results, seq, replayed)
    }

    /**
     * [post] off the calling thread, as a coroutine that cancelled closes its
     * connection: a read blocked in `HttpURLConnection` heeds no interrupt,
     * and a held claim given up went on waiting for the server's answer.
     */
    private suspend fun posting(path: String, type: String, payload: String, key: String?): Triple<Any, Long?, Boolean> =
        sending { sent ->
            val c = withContext(Dispatchers.IO) { open(path, type, key, sent) }
            suspendCancellableCoroutine { cont ->
                cont.invokeOnCancellation { c.disconnect() }
                Dispatchers.IO.asExecutor().execute { cont.resumeWith(runCatching { exchange(c, payload) }) }
            }
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
    private fun post(path: String, type: String, payload: String, key: String?): Triple<Any, Long?, Boolean> =
        sending { sent -> exchange(open(path, type, key, sent), payload) }

    /**
     * A request made by [request] -- handed the mark of a read near, or
     * null -- and its answer read. A read near is marked for the tenant's
     * read replica in the router's region, after the furthest position home
     * named to this connection, which the replica waits for: that change of
     * that history.
     */
    private inline fun sending(request: (Near?) -> Reply): Triple<Any, Long?, Boolean> {
        if (!reading || shared.reads != Reads.NEAREST) return finish(request(null), near = false)
        // A position of change 0 is none: nothing to wait for, as Go and
        // .NET send none.
        val sent = shared.position()?.takeIf { it.seq > 0 }
        val reply = request(Near(sent))
        // 504: no copy holds that write in the wait -- home had none
        // either, lost in a failover. Home says where it stands, which the
        // connection takes as its position, lower or not (`told`); asked
        // home again with no mark, the read is answered with what home holds.
        if (reply.status == 504 && sent != null) {
            val home = reply.v as? Row
            val at = home?.long("seq")
            if (home != null && at != null) shared.told(sent, Position(at, home.string("history")))
            return finish(request(null), near = false)
        }
        return finish(reply, near = true)
    }

    /** A read near's mark: the position it is sent after, null for none. */
    private class Near(val after: Position?)

    /**
     * An answer as it came: its status, its body read as JSON (null where
     * empty or not JSON) and the head it is read by. Read where the
     * exchange ran -- on `Dispatchers.IO` for a suspending call -- so a
     * large answer is never parsed on the caller's thread, an app's main
     * thread among them.
     */
    private class Reply(val status: Int, val v: Any?, val c: HttpURLConnection)

    private fun open(path: String, type: String, key: String?, near: Near?): HttpURLConnection {
        val c = URL("$url$path").openConnection() as HttpURLConnection
        c.requestMethod = "POST"
        c.connectTimeout = 10_000
        // A held request is answered once it writes, or at the end of its wait.
        c.readTimeout = 60_000 + (held ?: 0L).toInt()
        c.setRequestProperty("content-type", type)
        shared.token?.let { c.setRequestProperty("authorization", "Bearer $it") }
        (key ?: idempotencyKey)?.let { c.setRequestProperty("idempotency-key", it) }
        held?.takeIf { it > 0 }?.let { c.setRequestProperty("fenec-wait", it.toString()) }
        if (near != null) {
            c.setRequestProperty("fenec-read", "replica")
            near.after?.let { at ->
                c.setRequestProperty("fenec-after", at.seq.toString())
                at.history?.let { c.setRequestProperty("fenec-history", it) }
            }
        }
        c.doOutput = true
        return c
    }

    private fun exchange(c: HttpURLConnection, payload: String): Reply {
        val (status, body) = try {
            c.outputStream.use { it.write(payload.encodeToByteArray()) }
            val status = c.responseCode
            status to ((if (status >= 400) c.errorStream else c.inputStream)?.use { String(it.readBytes(), Charsets.UTF_8) } ?: "")
        } catch (e: IOException) {
            throw FenecException(FenecException.Code.IO, "the server could not be reached: ${e.message}")
        }
        parsing?.invoke()
        return Reply(status, runCatching { if (body.isEmpty()) null else Json.parse(body) }.getOrNull(), c)
    }

    private fun finish(reply: Reply, near: Boolean): Triple<Any, Long?, Boolean> {
        val status = reply.status
        val v = reply.v
        val c = reply.c
        if (status !in 200..299) throw refused(status, v as? Row, c.getHeaderField("retry-after"))
        v ?: throw FenecException(FenecException.Code.IO, "the server did not answer JSON ($status)")
        val seq = c.getHeaderField("fenec-seq")?.toLongOrNull()
        // A position from home alone: a read near's answer may be a
        // replica's, behind home or on a branch a failover left.
        if (seq != null && !near) shared.answered(Position(seq, c.getHeaderField("fenec-history")))
        return Triple(v, seq, c.getHeaderField("idempotent-replayed") == "true")
    }

    internal companion object {
        /**
         * Called on the thread an answer is about to be parsed on: the tests'
         * probe that a suspending call parses on `Dispatchers.IO`, not on its
         * caller's thread. Null, as it always is outside them.
         */
        @Volatile internal var parsing: (() -> Unit)? = null

        /**
         * A refusal as the error it is: its status and message, where a batch stopped, what a
         * `require n else '<reason>'` that refused it said, and where a tenant's quota refused it the
         * quota and how many seconds to wait -- to the millisecond where the body says it
         * (`retry_after_ms`), else `Retry-After`, or the same in the body.
         */
        fun refused(status: Int, r: Row?, retryAfter: String?): FenecException {
            val quota = r?.string("quota")
            val ms = r?.double("retry_after_ms")?.takeIf { it > 0 }?.let { it / 1000 }
            return FenecException(
                if (quota == null) code(status) else FenecException.Code.QUOTA,
                r?.string("error") ?: "HTTP $status", null,
                status = status, at = r?.int("at"), completed = r?.int("completed"),
                quota = quota,
                retryAfter = ms ?: retryAfter?.toDoubleOrNull() ?: r?.double("retry_after"),
                reason = r?.string("reason"),
            )
        }

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

/**
 * Where an answer from home left the tenant: its change (`Fenec-Seq`) and the
 * history that change is of (`Fenec-History`, `<term>-<id>`; none for a
 * database that never had a replica).
 */
internal data class Position(val seq: Long, val history: String?) {
    /** The history's term, one more at each promotion. */
    val term: Long get() = history?.substringBefore('-')?.toLongOrNull() ?: 0

    /**
     * The history's lineage: its id's top half, which every fork of the
     * tenant keeps and a tenant made again under its name does not.
     */
    val lineage: String? get() = history?.takeIf { '-' in it }?.substringAfter('-')?.takeIf { it.length >= 8 }?.take(8)

    /** Whether this is of another lineage than [q]: a tenant made again. */
    fun isOfAnotherLineage(q: Position): Boolean {
        val a = lineage
        val b = q.lineage
        return a != null && b != null && a != b
    }

    /**
     * Whether this is past [q]: of another lineage -- home's word on a
     * tenant made again, whose terms start again -- or a later term of the
     * same, or further in one.
     */
    fun isPast(q: Position?): Boolean =
        q == null || isOfAnotherLineage(q) || term > q.term || (term == q.term && seq > q.seq)
}

/**
 * A fenec-server over HTTP, with no local file: every query goes to the
 * server, through the same builder. With `reads = FenecRemote.Reads.NEAREST`
 * the builder's reads go to the tenant's read replica near the app, where a
 * fenec-shard router has one, after the app's own writes ([FenecRemote.Reads]).
 */
fun Fenec.Companion.connect(
    url: String,
    token: String? = null,
    reads: FenecRemote.Reads = FenecRemote.Reads.HOME,
): FenecRemote = FenecRemote(url, token, reads)
