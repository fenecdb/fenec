package com.fenecdb

import com.sun.net.httpserver.HttpExchange
import com.sun.net.httpserver.HttpServer
import kotlinx.coroutines.asCoroutineDispatcher
import kotlinx.coroutines.async
import kotlinx.coroutines.delay
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import java.io.File
import java.net.HttpURLConnection
import java.net.InetSocketAddress
import java.net.URL
import java.nio.file.Files
import java.util.Collections
import java.util.concurrent.CompletableFuture
import java.util.concurrent.CountDownLatch
import java.util.concurrent.ExecutionException
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.TimeoutException
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertNull
import kotlin.test.assertTrue

/**
 * A server binary started on a port of its own, found by the address its
 * `listening on:` line names: the binaries `cargo build -p fenec-server -p
 * fenec-shard` makes, named by `fenec.server` and `fenec.shard` (or
 * `FENEC_SERVER` and `FENEC_SHARD`), as `make kotlin-test` passes them.
 */
class Daemon(binary: String, args: List<String>) {
    private val process: Process = ProcessBuilder(listOf(binary) + args)
        .redirectOutput(ProcessBuilder.Redirect.DISCARD)
        .start()
    val port: Int

    init {
        // Its stderr read on a thread of its own to its end, the rest of its
        // output going nowhere; a binary that never says where it listens
        // is given up on after 20 s, as Swift's and Dart's harnesses do,
        // rather than hold the run on a `readLine` for good.
        val found = CompletableFuture<Int>()
        Thread {
            val seen = StringBuilder()
            runCatching {
                process.errorStream.bufferedReader().forEachLine { line ->
                    if (!found.isDone) {
                        seen.appendLine(line)
                        Regex("listening on: http://127\\.0\\.0\\.1:(\\d+) ").find(line)?.let { found.complete(it.groupValues[1].toInt()) }
                    }
                }
            }
            found.completeExceptionally(IllegalStateException("$binary did not start: $seen"))
        }.apply { isDaemon = true }.start()
        port = try {
            found.get(20, TimeUnit.SECONDS)
        } catch (e: TimeoutException) {
            process.destroyForcibly()
            throw IllegalStateException("$binary did not say where it listens in 20 s", e)
        } catch (e: ExecutionException) {
            process.destroyForcibly()
            throw e.cause ?: e
        }
    }

    /** Killed, as a crash would; returns once it is gone. */
    fun kill() {
        process.destroyForcibly().waitFor(10, TimeUnit.SECONDS)
    }

    companion object {
        fun binary(name: String) = System.getProperty("fenec.${name.removePrefix("fenec-")}")
            ?: System.getenv("FENEC_${name.removePrefix("fenec-").uppercase()}")
            ?: File("../../../target/debug/$name").absolutePath
    }
}

/**
 * eu1 in eu and us1 in us, the global router near eu and its standby near
 * us; tenant `acme` on eu1 with its read replica on us1. A client in us asks
 * the router there: [near].
 */
class ReadReplicas private constructor(val eu1: Daemon, val us1: Daemon, private val routers: List<Daemon>, private val dir: File) {
    /** us's router. */
    val near get() = "http://127.0.0.1:${routers[1].port}"

    /** The rows the replica on us1 holds, asked of its node directly. */
    fun replicaCount(): Long? {
        val (status, body) = call("http://127.0.0.1:${us1.port}", "POST", "/t/acme/query", """{"query":"get notes count"}""", read = true)
        if (status != 200) return null
        return ((Json.parse(body) as List<*>).first() as Row).long("count")
    }

    /** Everything killed, the files let go of. */
    fun stop() {
        (routers + listOf(us1, eu1)).forEach { it.kill() }
        dir.deleteRecursively()
    }

    companion object {
        suspend fun start(): ReadReplicas {
            val dir = Files.createTempDirectory("fenecdb-kotlin-replicas").toFile()
            val started = mutableListOf<Daemon>()
            try {
                fun node(n: String) = Daemon(
                    Daemon.binary("fenec-server"),
                    listOf("--dir", File(dir, n).path, "--http", "127.0.0.1:0", "--admin-token", "adm", "--replication-token", "repl"),
                ).also { started.add(it) }
                fun router(file: String, vararg extra: String) = Daemon(
                    Daemon.binary("fenec-shard"),
                    listOf("--listen", "127.0.0.1:0", "--directory", File(dir, file).path, "--token", "root", "--replication-token", "dirs") + extra,
                ).also { started.add(it) }
                val eu1 = node("eu1")
                val us1 = node("us1")
                val eu = router("global.fenec", "--near", "eu")
                val us = router("standby.fenec", "--near", "us", "--replica-of", "http://127.0.0.1:${eu.port}")
                val global = "http://127.0.0.1:${eu.port}"
                admin(global, "PUT", "/_shard/nodes/eu1", """{"addr":"127.0.0.1:${eu1.port}","token":"adm","region":"eu"}""")
                admin(global, "PUT", "/_shard/nodes/us1", """{"addr":"127.0.0.1:${us1.port}","token":"adm","region":"us"}""")
                admin(global, "PUT", "/_shard/tenants/acme", """{"node":"eu1","schema":"create collection notes (body text)"}""")
                val placed = Json.parse(admin(global, "PUT", "/_shard/tenants/acme/read-replicas/us", null)) as Row
                assertEquals("us1", placed.string("node"))
                // us's router follows the global directory: it routes to the
                // replica once its copy holds it.
                eventually("us's router knows of the read replica") {
                    val list = Json.parse(admin("http://127.0.0.1:${us.port}", "GET", "/_shard/tenants/acme/read-replicas", null)) as Row
                    list.list("read_replicas")!!.any { (it as Row).string("node") == "us1" }
                }
                return ReadReplicas(eu1, us1, listOf(eu, us), dir)
            } catch (e: Throwable) {
                started.forEach { it.kill() }
                dir.deleteRecursively()
                throw e
            }
        }

        /** A router's admin call under its token: the text it answers. */
        private fun admin(base: String, method: String, path: String, body: String?): String {
            val (status, text) = call(base, method, path, body, token = "root")
            check(status in 200..299) { "$method $path: $status $text" }
            return text
        }

        private fun call(base: String, method: String, path: String, body: String?, token: String? = null, read: Boolean = false): Pair<Int, String> {
            val c = URL("$base$path").openConnection() as HttpURLConnection
            c.requestMethod = method
            token?.let { c.setRequestProperty("authorization", "Bearer $it") }
            if (read) c.setRequestProperty("fenec-read", "replica")
            if (body != null) {
                c.doOutput = true
                c.outputStream.use { it.write(body.encodeToByteArray()) }
            }
            val status = c.responseCode
            val text = (if (status >= 400) c.errorStream else c.inputStream)?.use { String(it.readBytes(), Charsets.UTF_8) } ?: ""
            return status to text
        }
    }
}

/** One answer of a script: its status, body and head, given once [gate] opens where it has one -- two requests under way at once. */
data class Scripted(
    val status: Int = 200,
    val body: String = "[]",
    val seq: String? = null,
    val history: String? = null,
    val gate: CountDownLatch? = null,
)

/**
 * A server between a connection and where its requests go, keeping each
 * request's head in the order they came, as it arrives: passed on to
 * [target], or -- with none -- answered from the script in turn, `[]` once
 * it runs out.
 */
class Wire(private val target: String?, script: List<Scripted> = emptyList()) : AutoCloseable {
    private val script = ArrayDeque(script)
    private val heads = mutableListOf<Map<String, String>>()
    private val pool: ExecutorService = Executors.newCachedThreadPool()
    private val server = HttpServer.create(InetSocketAddress("127.0.0.1", 0), 0).apply {
        executor = pool
        createContext("/") { ex -> runCatching { take(ex) }.onFailure { ex.close() } }
        start()
    }

    val url get() = "http://127.0.0.1:${server.address.port}"

    /** Every request's head so far, its names in lower case. */
    val all: List<Map<String, String>> @Synchronized get() = heads.toList()

    /** The last request's head. */
    val last: Map<String, String> get() = all.last()

    /** Answers for the script's end. */
    @Synchronized fun add(vararg more: Scripted) {
        script.addAll(more)
    }

    private fun take(ex: HttpExchange) {
        val head = ex.requestHeaders.entries.associate { (k, v) -> k.lowercase() to v.joinToString(",") }
        val answer = synchronized(this) {
            heads.add(head)
            if (target == null) script.removeFirstOrNull() ?: Scripted() else null
        }
        val body = ex.requestBody.use { it.readBytes() }
        if (answer != null) {
            answer.gate?.await()
            ex.responseHeaders.add("content-type", "application/json")
            answer.seq?.let { ex.responseHeaders.add("fenec-seq", it) }
            answer.history?.let { ex.responseHeaders.add("fenec-history", it) }
            val bytes = answer.body.encodeToByteArray()
            ex.sendResponseHeaders(answer.status, bytes.size.toLong())
            ex.responseBody.use { it.write(bytes) }
            return
        }
        val c = URL(target + ex.requestURI).openConnection() as HttpURLConnection
        c.requestMethod = ex.requestMethod
        ex.requestHeaders.forEach { (name, values) -> if (name.lowercase() !in HOP) values.forEach { c.addRequestProperty(name, it) } }
        if (body.isNotEmpty()) {
            c.doOutput = true
            c.outputStream.use { it.write(body) }
        }
        val status = c.responseCode
        val back = (if (status >= 400) c.errorStream else c.inputStream)?.use { it.readBytes() } ?: ByteArray(0)
        c.headerFields.forEach { (name, values) -> if (name != null && name.lowercase() !in HOP) values.forEach { ex.responseHeaders.add(name, it) } }
        ex.sendResponseHeaders(status, if (back.isEmpty()) -1 else back.size.toLong())
        ex.responseBody.use { it.write(back) }
    }

    override fun close() {
        server.stop(0)
        pool.shutdownNow()
    }

    private companion object {
        val HOP = setOf("host", "content-length", "connection", "transfer-encoding", "keep-alive")
    }
}

/** Waits until [done] holds, or fails after ten seconds. */
private suspend fun eventually(what: String, done: suspend () -> Boolean) {
    runCatching { withTimeout(10_000) { while (!done()) delay(10) } }
    assertTrue(done(), "timed out waiting for $what")
}

/**
 * Reads near a tenant's read replica (`Fenec.connect(url, reads =
 * Reads.NEAREST)`): against real routers and nodes, and what a connection
 * sends after its answers against a script -- a failover's 504, answers
 * arriving out of order, a tenant made again.
 */
class ReadsTest {
    private val h1 = "1-00000000000000aa"
    private val h2 = "2-00000000000000bb"

    private fun written(seq: String, history: String) = Scripted(body = """{"affected":1}""", seq = seq, history = history)

    private fun refused(seq: Long, history: String, gate: CountDownLatch? = null) =
        Scripted(status = 504, body = """{"error":"not in this copy","seq":$seq,"history":"$history"}""", gate = gate)

    /**
     * The builder's reads, and an `asRead()` copy's, are answered by the
     * replica near the app -- shown by an answer while home cannot be
     * reached, which only the replica could give -- and each is sent after
     * the connection's last write, whichever copy wrote it, so it never
     * reads older than what it wrote; writes, and every request of a
     * connection reading home, go home.
     */
    @Test
    fun readsNearAReadReplicaAreAnsweredThereNeverOlderThanTheAppWrote() = runBlocking {
        val w = ReadReplicas.start()
        val sent = Wire(w.near)
        try {
            val db = Fenec.connect("${sent.url}/t/acme", reads = FenecRemote.Reads.NEAREST)
            val home = Fenec.connect("${w.near}/t/acme")

            // A write goes home, unmarked -- marked, it would be refused --
            // and says where it left the tenant.
            assertEquals(1L, db.from("notes").insert(mapOf("body" to "a")))
            val a = db.seq!!
            assertNull(sent.last["fenec-read"])
            assertNull(sent.last["fenec-after"])
            // The builder's read after it waits for it on the replica: that
            // change of the history the write named, which a node that
            // replicates gives each tenant.
            assertEquals(listOf("a"), db.from("notes").rows().map { it.string("body") })
            assertEquals("replica", sent.last["fenec-read"])
            assertEquals("$a", sent.last["fenec-after"])
            val history = sent.last["fenec-history"]!!
            assertTrue(Regex("^\\d+-[0-9a-f]{16}$").matches(history), history)

            // The copies share the position: a keyed copy's write is what
            // the connection's next read is sent after, and an `asRead()`
            // copy's reads go after the connection's writes.
            assertEquals(1L, db.withIdempotencyKey("note-b").from("notes").insert(mapOf("body" to "b")))
            val b = db.seq!!
            assertTrue(b > a)
            assertNull(sent.last["fenec-read"])
            assertEquals("note-b", sent.last["idempotency-key"])
            assertEquals(2L, db.from("notes").count())
            assertEquals("replica", sent.last["fenec-read"])
            assertEquals("$b", sent.last["fenec-after"])
            val reader = db.asRead()
            val both = reader.batch(listOf(db.from("notes").where("body", "a").toFenecQL(), db.from("notes").where("body", "b").toFenecQL()))
            assertEquals(listOf(1, 1), both.results.map { it.rows.size })
            assertEquals("replica", sent.last["fenec-read"])
            assertEquals("$b", sent.last["fenec-after"])
            assertEquals(2L, reader.query("get notes count").first().long("count"))
            assertEquals("replica", sent.last["fenec-read"])
            assertEquals(history, sent.last["fenec-history"])
            // A text not marked goes home.
            db.query("get notes count")
            assertNull(sent.last["fenec-read"])
            assertNull(sent.last["fenec-after"])
            // A write marked by mistake is refused, never landed.
            assertEquals(403, assertFailsWith<FenecException> { reader.execute("""put notes {body: "x"}""") }.status)

            // Home gone: the reads near are still answered -- by the
            // replica, the one copy left -- and the connection reading home
            // is not. The replica asked itself first, until it holds both: a
            // read near above may have been answered home, the replica not
            // caught up yet.
            eventually("the read replica holds both writes") { w.replicaCount() == 2L }
            w.eu1.kill()
            assertEquals(listOf("a", "b"), db.from("notes").order("body").rows().map { it.string("body") })
            assertEquals("replica", sent.last["fenec-read"])
            assertEquals("$b", sent.last["fenec-after"])
            assertEquals(502, assertFailsWith<FenecException> { home.from("notes").rows() }.status)
            assertEquals(502, assertFailsWith<FenecException> { db.from("notes").insert(mapOf("body" to "c")) }.status)
        } finally {
            sent.close()
            w.stop()
        }
    }

    /** A connection reading home marks nothing, `asRead()` or not. */
    @Test
    fun aConnectionReadsHomeUnlessItAsksForNearest() = runBlocking {
        Wire(null).use { sent ->
            val db = Fenec.connect(sent.url)
            db.from("t").rows()
            db.asRead().query("get t count")
            assertEquals(2, sent.all.size)
            sent.all.forEach {
                assertNull(it["fenec-read"])
                assertNull(it["fenec-after"])
            }
            assertEquals(FenecRemote.Reads.HOME, db.reads)
        }
    }

    /**
     * Reads near go after the furthest position an answer from home named,
     * through any copy -- not the last answered, and not a read near's own
     * answer, which may be a replica's. A read no copy holds that for (504)
     * is asked home with neither, and home's position, which the 504 names,
     * is the connection's from then.
     */
    @Test
    fun readsNearAreSentAfterTheFurthestPositionFromHome() = runBlocking {
        val script = listOf(
            written("9", h1), written("12", h1), written("10", h1),
            Scripted(body = """[{"n":1}]"""), Scripted(body = """[{"count":2}]"""), Scripted(body = """[{"count":2}]"""),
            Scripted(body = """{"results":[]}""", seq = "15", history = h1),
            Scripted(status = 504, body = """{"error":"change 12 is not in this copy's history","seq":10,"history":"$h2"}"""),
            Scripted(body = """[{"n":1}]"""), Scripted(body = """[{"n":1}]"""),
        )
        Wire(null, script).use { sent ->
            val db = Fenec.connect(sent.url, reads = FenecRemote.Reads.NEAREST)
            db.withIdempotencyKey("k1").execute("put t {n: 1}")
            db.withWait(1000).execute("set t {n: 2} where n = 1")
            db.execute("put t {n: 3}")
            db.from("t").rows()
            assertEquals(2L, db.from("t").count())
            db.query("get t count")
            db.asRead().batch(listOf(db.from("t").toFenecQL()))
            // The furthest of the three writes, and not the read near's 15.
            assertEquals(12L, db.seq)
            assertEquals(1, db.withWait(1000).from("t").rows().size)
            db.from("t").rows()

            val seen = sent.all
            assertEquals(10, seen.size)
            seen.take(3).forEach {
                assertNull(it["fenec-read"])
                assertNull(it["fenec-after"])
            }
            // The furthest position, 12, not the last answered, 10.
            for (i in listOf(3, 4, 6, 7)) {
                assertEquals("replica", seen[i]["fenec-read"], "request $i")
                assertEquals("12", seen[i]["fenec-after"], "request $i")
                assertEquals(h1, seen[i]["fenec-history"], "request $i")
            }
            for (i in listOf(5, 8)) {
                assertNull(seen[i]["fenec-read"], "request $i")
                assertNull(seen[i]["fenec-after"], "request $i")
                assertNull(seen[i]["fenec-history"], "request $i")
            }
            // Home's position taken, lower and of the history after the
            // failover.
            assertEquals("10", seen[9]["fenec-after"])
            assertEquals(h2, seen[9]["fenec-history"])
            assertEquals(10L, db.seq)
        }
    }

    /**
     * `seq` is the furthest position home named, as Go's `Seq()` and .NET's
     * `Seq` are: a write answered late does not move it back, and a read
     * near's 504 sets it to where home says it stands. A position of change
     * 0 marks a read near and names no change to wait for.
     */
    @Test
    fun seqIsTheFurthestPositionHomeNamed() = runBlocking {
        val h3 = "3-00000000000000cc"
        val held = CountDownLatch(1)
        val script = listOf(
            Scripted(body = """{"results":[]}""", seq = "0", history = h1), Scripted(),
            written("101", h2).copy(gate = held), written("102", h2),
            Scripted(), refused(7, h3), Scripted(), Scripted(),
        )
        Wire(null, script).use { sent ->
            val db = Fenec.connect(sent.url, reads = FenecRemote.Reads.NEAREST)
            db.batch(listOf(db.from("t").toFenecQL()))
            assertEquals(0L, db.seq)
            db.from("t").rows()
            assertEquals("replica", sent.last["fenec-read"])
            assertNull(sent.last["fenec-after"])
            assertNull(sent.last["fenec-history"])

            // Two writes, the first answered after the second.
            val first = async { db.execute("put t {n: 1}") }
            eventually("the first write sent") { sent.all.size == 3 }
            db.execute("put t {n: 2}")
            assertEquals(102L, db.seq)
            held.countDown()
            first.await()
            assertEquals(102L, db.seq)
            db.from("t").rows()
            assertEquals("102", sent.last["fenec-after"])

            // A 504 naming a later term's 7: home's word, lower or not.
            db.from("t").rows()
            assertEquals(7L, db.seq)
            db.from("t").rows()
            assertEquals("7", sent.last["fenec-after"])
            assertEquals(h3, sent.last["fenec-history"])
        }
    }

    /**
     * A suspending call reads its answer on `Dispatchers.IO`, as the README
     * says, never on its caller's thread -- an app's main thread among them.
     * Parsed on the caller's, a large answer held a `viewModelScope` call's
     * main thread for as long as it took.
     */
    @Test
    fun anAnswerIsParsedOffTheCallersThread() {
        val caller = Executors.newSingleThreadExecutor { r -> Thread(r, "the caller") }
        val dispatcher = caller.asCoroutineDispatcher()
        val parsedOn = Collections.synchronizedList(mutableListOf<String>())
        FenecRemote.parsing = { parsedOn.add(Thread.currentThread().name) }
        try {
            Wire(null, listOf(written("1", h1), Scripted(body = """[{"n":1}]"""), refused(1, h1), Scripted())).use { sent ->
                runBlocking(dispatcher) {
                    val db = Fenec.connect(sent.url, reads = FenecRemote.Reads.NEAREST)
                    db.execute("put t {n: 1}")
                    db.from("t").rows()
                    db.from("t").rows()
                }
            }
            assertEquals(4, parsedOn.size, "$parsedOn")
            assertTrue(parsedOn.none { it == "the caller" }, "$parsedOn")
        } finally {
            FenecRemote.parsing = null
            dispatcher.close()
        }
    }

    /**
     * A read near's 504 sets the position back only over the one the read
     * was sent after -- a write answered meanwhile is further, and set back
     * the next read near would miss it -- and never to an earlier term,
     * which a fenced old primary names.
     */
    @Test
    fun a504SetsThePositionBackOverItsOwnAloneNeverToAnEarlierTerm() = runBlocking {
        Wire(null, listOf(written("100", h2))).use { sent ->
            val db = Fenec.connect(sent.url, reads = FenecRemote.Reads.NEAREST)
            db.execute("put t {n: 1}")
            // A read near is sent after 100; a write lands at 101 while it
            // waits; then home answers the read 504 at 100.
            val held = CountDownLatch(1)
            sent.add(refused(100, h2, held))
            val read = async { db.from("t").rows() }
            eventually("the read sent") { sent.all.size == 2 }
            assertEquals("100", sent.last["fenec-after"])
            sent.add(written("101", h2))
            db.execute("put t {n: 2}")
            held.countDown()
            read.await()
            db.from("t").rows()
            assertEquals("101", sent.last["fenec-after"])

            // A 504 naming an earlier term -- the old primary, fenced,
            // reached through a router behind a failover -- is not taken.
            sent.add(refused(140, h1))
            db.from("t").rows()
            db.from("t").rows()
            assertEquals("101", sent.last["fenec-after"])
            assertEquals(h2, sent.last["fenec-history"])
        }
    }

    /**
     * A tenant made again under its name starts again at term 1, of another
     * lineage -- the top half of its history's id: home's position is taken,
     * from a 504 and from an answer, its term lower. Refused as an earlier
     * term, every read near waited its whole wait for good. Within one
     * lineage an earlier term is still refused.
     */
    @Test
    fun aTenantMadeAgainHasItsPositionTakenItsTermLower() = runBlocking {
        val old = "3-aaaaaaaa000000c3"
        val remade = "1-bbbbbbbb00000001"
        val third = "1-cccccccc00000001"
        val second = "2-cccccccc00000002"
        val script = listOf(
            written("50", old), refused(2, remade), Scripted(), Scripted(),
            written("1", third), Scripted(), written("9", second), written("90", third), Scripted(),
        )
        Wire(null, script).use { sent ->
            val db = Fenec.connect(sent.url, reads = FenecRemote.Reads.NEAREST)
            db.execute("put t {n: 1}")
            db.from("t").rows()
            db.from("t").rows()
            assertEquals("2", sent.last["fenec-after"])
            assertEquals(remade, sent.last["fenec-history"])
            db.execute("put t {n: 2}")
            db.from("t").rows()
            assertEquals("1", sent.last["fenec-after"])
            assertEquals(third, sent.last["fenec-history"])
            db.execute("put t {n: 3}")
            db.execute("put t {n: 4}")
            db.from("t").rows()
            assertEquals("9", sent.last["fenec-after"])
            assertEquals(second, sent.last["fenec-history"])
        }
    }
}
