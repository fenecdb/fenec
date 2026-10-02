package com.fenecdb

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withContext
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import java.security.SecureRandom
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledExecutorService
import java.util.concurrent.TimeUnit
import kotlin.coroutines.resume

/**
 * The rows of one collection a replica keeps: a filter the server applies,
 * in the JS sync layer's form -- `mapOf("status" to "open", "priority" to
 * mapOf("gte" to 3))` -- the fields it carries, and the business key an
 * optimistic insert is matched with the server's copy by (a `text @hash`
 * field).
 */
data class Shape @JvmOverloads constructor(
    val collection: String,
    val where: Map<String, Any?>? = null,
    val select: List<String>? = null,
    val key: String? = null,
) {
    internal fun json(): Map<String, Any?> = buildMap {
        put("collection", collection)
        where?.let { put("where", Values.normalize(it)) }
        select?.let { put("select", it) }
        key?.let { put("key", it) }
    }
}

/** Where a replica stands with its server. */
data class SyncStatus(
    val state: State,
    /** Writes the server has not answered. */
    val pending: Int,
    /** The last error: a connection's until it is back, a refused write's until the next. */
    val error: Failure?,
    val shapes: List<ShapeState>,
) {
    enum class State {
        /** Every shape's stream is open and seeded. */
        ONLINE,

        /** The server cannot be reached, or the app said the network is gone. */
        OFFLINE,

        /** Connecting, seeding, or sending the writes left from before. */
        CATCHING_UP,
    }

    /** An error, and its HTTP status (0: the network, or the replica). */
    data class Failure(val message: String, val status: Int)

    data class ShapeState(val collection: String, val cursor: Long, val seeded: Boolean, val connected: Boolean)

    /** Whether every shape has had its seed. */
    val seeded: Boolean get() = shapes.isNotEmpty() && shapes.all { it.seeded }

    internal companion object {
        val STARTING = SyncStatus(State.CATCHING_UP, 0, null, emptyList())

        fun of(v: Row) = SyncStatus(
            when (v.string("state")) {
                "online" -> State.ONLINE
                "offline" -> State.OFFLINE
                else -> State.CATCHING_UP
            },
            v.int("pending") ?: 0,
            v.row("error")?.let { Failure(it.string("message") ?: "", it.int("status") ?: 0) },
            (v.list("shapes") ?: emptyList()).map {
                val s = it as Row
                ShapeState(s.string("collection") ?: "", s.long("cursor") ?: 0, s.bool("seeded") == true, s.bool("connected") == true)
            },
        )
    }
}

/** A write the server refused: it was put back on the replica. */
data class Refusal(val status: Int, val message: String, val query: String)

/**
 * A replica of [shapes] in the file at [path], kept in step with the server
 * at [url]: reads and live queries are the file's, a write to a shape's
 * collection is applied at once and sent to the server -- queued in the
 * file while it cannot be reached, and sent under an idempotency key, so it
 * lands once. The same [Fenec]: `from`, `live` and `query` work as they do
 * over a file of the app's own.
 *
 * ```kotlin
 * val db = Fenec.sync(
 *     url = "https://api.example.com", token = jwt,
 *     shapes = listOf(Shape("todos", where = mapOf("done" to false), key = "key")),
 *     path = File(context.filesDir, "todos.fenec").path,
 * )
 * ```
 *
 * The requests and the change stream are `HttpURLConnection`'s, the JDK's
 * and Android's own, with the system's TLS: Android refuses cleartext HTTP
 * by default, and a server is reached through TLS in front of it.
 * [tokenProvider] is asked for a fresh token when the server answers 401.
 */
suspend fun Fenec.Companion.sync(
    url: String,
    token: String? = null,
    shapes: List<Shape>,
    path: String,
    flags: Int = 0,
    tokenProvider: (suspend () -> String)? = null,
): Fenec = withContext(Dispatchers.IO) {
    val db = Fenec.open(path, flags)
    try {
        val replica = Replica(db, url, token, tokenProvider)
        val seed = SecureRandom().let { r -> "%016x%016x".format(r.nextLong(), r.nextLong()) }
        val config = Json.write(
            buildMap {
                put("url", url)
                token?.let { put("token", it) }
                put("seed", seed)
                put("shapes", shapes.map { it.json() })
            },
        )
        val first = FenecNative.answer(FenecNative.syncStart(db.handle, config.encodeToByteArray()))
        db.attach(replica)
        replica.start(first)
        db
    } catch (e: Throwable) {
        db.close()
        throw e
    }
}

/**
 * A replica's sync with its server: its status as a [StateFlow], its
 * token, and the network as the app sees it. Its work is on a thread of its
 * own, where every request's answer, every piece of a stream and every
 * timer is handed in turn, so the engine is told of a stream's bytes in
 * their order and an action is never performed before the one it cancels.
 */
class Replica internal constructor(
    private val db: Fenec,
    url: String,
    @Volatile private var token: String?,
    private val provider: (suspend () -> String)?,
) {
    private val url = url.trimEnd('/')
    private val handle = db.handle
    private val queue: ScheduledExecutorService = Executors.newSingleThreadScheduledExecutor { r ->
        Thread(r, "fenecdb-sync").apply { isDaemon = true }
    }
    private val connections = HashMap<Long, HttpURLConnection>()
    private val current = MutableStateFlow(SyncStatus.STARTING)
    private val refused = MutableSharedFlow<Refusal>(extraBufferCapacity = 64)

    @Volatile private var stopped = false

    /** The status now and at every change; `status.value` never waits. */
    val status: StateFlow<SyncStatus> = current.asStateFlow()

    /** The writes the server refused, each put back on the replica. */
    val refusals: SharedFlow<Refusal> = refused.asSharedFlow()

    /** Returns once every shape has had its seed: at once for a replica opened again. */
    suspend fun ready() {
        refresh()
        current.first { it.seeded }
    }

    /** Returns once the server has answered every write made so far. */
    suspend fun pushed() {
        refresh()
        current.first { it.pending == 0 }
    }

    /** The status once everything told the sync before this call is in it -- a write just made among its pending ones. */
    suspend fun refresh(): SyncStatus = suspendCancellableCoroutine { cont ->
        queue.execute {
            reload()
            cont.resume(current.value)
        }
    }

    /** A fresh token, for the requests and streams from here on. */
    fun setToken(token: String) {
        this.token = token
        signal(mapOf("token" to token))
    }

    /**
     * The network as the platform sees it (`ConnectivityManager`): offline
     * closes the streams and sends nothing; online comes back at once, the
     * backoff forgotten.
     */
    fun setOnline(online: Boolean) = signal(mapOf("online" to online))

    /** What an app calls in `onStart`: the streams a stop cut are opened again at once, from their cursors. */
    fun resume() = setOnline(true)

    /** The server itself, for what the replica does not hold. */
    val remote: FenecRemote get() = FenecRemote(url, token)

    // ------------------------------------------------------------- the engine

    internal fun start(first: String) = queue.execute { perform(first) }

    /** Asks for what a write left due. */
    internal fun poll() {
        if (!stopped) queue.execute { feed(KIND_POLL, 0) }
    }

    internal fun stop() {
        if (stopped) return
        runCatching {
            queue.submit {
                feed(KIND_SIGNAL, 0, bytes = """{"stop":true}""".encodeToByteArray())
                stopped = true
                connections.values.forEach { it.disconnect() }
                connections.clear()
            }.get()
        }
        queue.shutdownNow()
    }

    private fun signal(fields: Map<String, Any?>) {
        val bytes = Json.write(fields).encodeToByteArray()
        if (!stopped) queue.execute { feed(KIND_SIGNAL, 0, bytes = bytes) }
    }

    private fun later(f: () -> Unit) {
        if (!stopped) runCatching { queue.execute(f) }
    }

    /** Tells the engine, on the queue, and performs what it answers. */
    private fun feed(kind: Int, id: Long, status: Int = 0, seq: Long = 0, bytes: ByteArray? = null) {
        if (stopped) return
        val out = runCatching { FenecNative.answer(FenecNative.syncFeed(handle, kind, id, status, seq, bytes)) }.getOrNull()
        if (out != null) perform(out)
    }

    private fun perform(json: String) {
        val actions = runCatching { Json.parse(json) as List<*> }.getOrNull() ?: return
        for (a in actions) {
            val r = a as Row
            val id = r.long("id") ?: 0
            when (r.string("do")) {
                "request" -> request(id, r)
                "stream" -> stream(id, r)
                "cancel" -> connections.remove(id)?.let { c -> Thread { c.disconnect() }.start() }
                "wait" -> queue.schedule({ feed(KIND_TIMER, id) }, r.long("ms") ?: 0, TimeUnit.MILLISECONDS)
                "token" -> provider?.let { p ->
                    Thread {
                        runCatching { kotlinx.coroutines.runBlocking { p() } }.getOrNull()?.let { setToken(it) }
                    }.start()
                }
                "changed" -> db.lives.touch()
                "refused" -> refused.tryEmit(Refusal(r.int("status") ?: 0, r.string("message") ?: "", r.string("query") ?: ""))
                "status" -> reload()
            }
        }
    }

    private fun reload() {
        val out = runCatching { FenecNative.answer(FenecNative.syncStatus(handle)) }.getOrNull() ?: return
        current.value = SyncStatus.of(Json.parse(out) as Row)
    }

    private fun connection(r: Row, readTimeout: Int): HttpURLConnection {
        val c = URL(r.string("url")).openConnection() as HttpURLConnection
        c.requestMethod = r.string("method") ?: "GET"
        c.connectTimeout = 10_000
        c.readTimeout = readTimeout
        c.useCaches = false
        r.row("headers")?.forEach { (k, v) -> c.setRequestProperty(k, v as String) }
        return c
    }

    /** A request on a thread of its own, its answer handed to the queue. */
    private fun request(id: Long, r: Row) {
        val c = connection(r, 30_000)
        connections[id] = c
        Thread {
            var status = 0
            var seq = 0L
            val body: ByteArray = try {
                r.string("body")?.let { b ->
                    c.doOutput = true
                    c.outputStream.use { it.write(b.encodeToByteArray()) }
                }
                status = c.responseCode
                seq = c.getHeaderField("Fenec-Seq")?.toLongOrNull() ?: 0
                (if (status >= 400) c.errorStream else c.inputStream)?.use { it.readBytes() } ?: ByteArray(0)
            } catch (e: IOException) {
                status = 0
                (e.message ?: e.toString()).encodeToByteArray()
            }
            later {
                connections.remove(id)
                feed(KIND_RESPONSE, id, status, seq, body)
            }
        }.apply { isDaemon = true }.start()
    }

    /** A stream on a thread of its own, each piece handed to the queue as it comes. */
    private fun stream(id: Long, r: Row) {
        // The server's keepalive comes every 20 s: a silence past it is a
        // connection gone.
        val c = connection(r, 90_000)
        connections[id] = c
        Thread {
            var why = ""
            try {
                val status = c.responseCode
                if (status != 200) {
                    val body = c.errorStream?.use { it.readBytes() } ?: ByteArray(0)
                    later { feed(KIND_OPENED, id, status, bytes = body) }
                    return@Thread
                }
                later { feed(KIND_OPENED, id, 200) }
                c.inputStream.use { input ->
                    val buf = ByteArray(16 * 1024)
                    while (true) {
                        val n = input.read(buf)
                        if (n < 0) break
                        val piece = buf.copyOf(n)
                        later { feed(KIND_BYTES, id, bytes = piece) }
                    }
                }
            } catch (e: IOException) {
                why = e.message ?: e.toString()
            }
            later { feed(KIND_CLOSED, id, bytes = why.encodeToByteArray()) }
        }.apply { isDaemon = true }.start()
    }

    private companion object {
        const val KIND_POLL = 0
        const val KIND_RESPONSE = 1
        const val KIND_OPENED = 2
        const val KIND_BYTES = 3
        const val KIND_CLOSED = 4
        const val KIND_TIMER = 5
        const val KIND_SIGNAL = 6
    }
}

/** A replica's sync, when the database is one ([Fenec.Companion.sync]). */
val Fenec.replica: Replica? get() = syncing
