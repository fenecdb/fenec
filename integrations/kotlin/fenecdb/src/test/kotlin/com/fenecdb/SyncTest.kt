package com.fenecdb

import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import java.io.File
import java.nio.file.Files
import java.util.Collections
import java.util.concurrent.TimeUnit
import kotlin.test.AfterTest
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertTrue

/**
 * A fenec-server the tests start, kill and start again on its port: the
 * binary `cargo build -p fenec-server` makes, named by `fenec.server` (or
 * `FENEC_SERVER`), as `make kotlin-test` passes it.
 */
class Server(name: String, private val tenantRows: Int? = null) {
    private val dir: File = Files.createTempDirectory("fenecdb-kotlin-$name").toFile()
    private val file = File(dir, "server.fenec").path
    private var process: Process? = null
    var port = 0
        private set

    /** With [tenantRows], a tenant node (`--dir`) holding each tenant to that many rows: tenant `acme`. */
    val url get() = "http://127.0.0.1:$port" + (if (tenantRows == null) "" else "/t/acme")

    init {
        start(0)
        if (tenantRows != null) {
            val c = java.net.URL("http://127.0.0.1:$port/_admin/tenants/acme").openConnection() as java.net.HttpURLConnection
            c.requestMethod = "PUT"
            c.setRequestProperty("Authorization", "Bearer adm")
            check(c.responseCode == 201) { "the tenant was not made: ${c.responseCode}" }
            c.disconnect()
        }
        run("create collection tasks (key text @unique, title text, status text @hash, priority int)")
        run(
            """put tasks [{key: "a", title: "one", status: "open", priority: 1},
                          {key: "b", title: "two", status: "open", priority: 5},
                          {key: "c", title: "three", status: "closed", priority: 3}]""",
        )
    }

    fun start(port: Int) {
        val binary = System.getProperty("fenec.server") ?: System.getenv("FENEC_SERVER")
            ?: File("../../../target/debug/fenec-server").absolutePath
        val at = when (tenantRows) {
            null -> listOf("--file", file)
            else -> listOf("--dir", file, "--admin-token", "adm", "--tenant-max-rows", "$tenantRows")
        }
        val p = ProcessBuilder(listOf(binary, "--http", "127.0.0.1:$port", "--sync", "always") + at)
            .redirectOutput(ProcessBuilder.Redirect.DISCARD)
            .start()
        process = p
        val err = p.errorStream.bufferedReader()
        val seen = StringBuilder()
        while (true) {
            val line = err.readLine() ?: throw IllegalStateException("fenec-server did not start: $seen")
            seen.appendLine(line)
            val m = Regex("listening on: http://127\\.0\\.0\\.1:(\\d+) ").find(line) ?: continue
            this.port = m.groupValues[1].toInt()
            // The rest of its output goes nowhere.
            Thread { runCatching { err.readText() } }.apply { isDaemon = true }.start()
            return
        }
    }

    /** Killed, as a crash would: no checkpoint, no goodbye. */
    fun kill() {
        process?.destroyForcibly()?.waitFor(10, TimeUnit.SECONDS)
        process = null
    }

    /** Started again over its file, on the port it had. */
    fun restart() = start(port)

    fun run(text: String): Answer = Fenec.connect(url).runBlocking(text)

    fun stop() {
        kill()
        dir.deleteRecursively()
    }
}

class SyncTest {
    private val servers = mutableListOf<Server>()
    private val open = Shape("tasks", where = mapOf("status" to "open"), key = "key")

    private fun server(name: String) = Server(name).also { servers.add(it) }

    private fun path(name: String) = File(Files.createTempDirectory("fenecdb-kotlin-replica-$name").toFile(), "app.fenec").path

    @AfterTest
    fun stop() = servers.forEach { it.stop() }

    /** Waits until [done] holds, or fails after ten seconds. */
    private suspend fun eventually(what: String, done: suspend () -> Boolean) {
        runCatching { withTimeout(10_000) { while (!done()) delay(10) } }
        assertTrue(done(), "timed out waiting for $what")
    }

    /**
     * The replica offline before the next write: `setOnline` puts the
     * signal on the sync's queue, and an event already on it -- a stream's
     * bytes, a status -- runs the engine first, which sends what a write
     * made meanwhile queued. The quota test's insert went out and was
     * refused and put back before the read that was to find it ([one]), 8
     * runs in 593 in containers of three CPUs, four at once beside four
     * busy loops, none in 595 since: `refresh` returns once the queue has
     * reached the signal.
     */
    private suspend fun offline(db: Fenec) {
        db.replica!!.setOnline(false)
        assertEquals(SyncStatus.State.OFFLINE, db.replica!!.refresh().state)
    }

    private suspend fun titles(db: Fenec) = db.from("tasks").select("title").order("title").rows().map { it.string("title") }

    @Test
    fun theSeedFillsTheReplicaWithTheShape() = runBlocking {
        val s = server("seed")
        val db = Fenec.sync(url = s.url, shapes = listOf(open), path = path("seed"))
        db.replica!!.ready()
        assertEquals(listOf("one", "two"), titles(db))
        assertEquals(SyncStatus.State.ONLINE, db.replica!!.refresh().state)
        // Reads are the file's: the server gone, they go on.
        s.kill()
        assertEquals(2L, db.from("tasks").count())
        db.close()
    }

    @Test
    fun aLiveQueryRunsAgainOnAServerWrite() = runBlocking {
        val s = server("live")
        val db = Fenec.sync(url = s.url, shapes = listOf(open), path = path("live"))
        db.replica!!.ready()
        val seen = Collections.synchronizedList(ArrayList<List<String?>>())
        val job = launch(Dispatchers.IO) {
            db.live(db.from("tasks").select("title").order("title")).collect { rows -> seen.add(rows.map { it.string("title") }) }
        }
        eventually("the first rows") { seen.size >= 1 }
        s.run("""put tasks {key: "d", title: "four", status: "open"}""")
        eventually("the server's write") { seen.lastOrNull() == listOf("four", "one", "two") }
        job.cancel()
        db.close()
    }

    @Test
    fun anOptimisticWriteShowsAtOnceAndLands() = runBlocking {
        val s = server("optimistic")
        val db = Fenec.sync(url = s.url, shapes = listOf(open), path = path("optimistic"))
        db.replica!!.ready()
        // Offline, nothing is sent: the server's answer cannot replace the
        // row before it is read (see aRefusedWriteIsPutBack).
        offline(db)
        db.from("tasks").insert(mapOf("title" to "new", "status" to "open", "priority" to 2))
        // At once, under a temporary id.
        assertTrue((db.from("tasks").where("title", "new").first()!!.long("id") ?: 0) >= 1L shl 52)
        db.replica!!.setOnline(true)
        db.replica!!.pushed()
        eventually("the server's copy in place of the temporary row") {
            val rows = db.from("tasks").where("title", "new").rows()
            rows.size == 1 && rows[0].long("id")!! < 1L shl 52
        }
        assertEquals(1, s.run("""get tasks where title = "new"""").rows.size)
        // An update and a delete go the same way.
        db.from("tasks").where("key", "a").update(mapOf("title" to "ONE"))
        db.from("tasks").where("key", "b").delete()
        db.replica!!.pushed()
        assertEquals(1, s.run("""get tasks where title = "ONE"""").rows.size)
        assertEquals(0, s.run("""get tasks where key = "b"""").rows.size)
        db.close()
    }

    @Test
    fun aRefusedWriteIsPutBack() = runBlocking {
        val s = server("refused")
        val db = Fenec.sync(url = s.url, shapes = listOf(open), path = path("refused"))
        db.replica!!.ready()
        val refused = Collections.synchronizedList(ArrayList<Refusal>())
        // Subscribed before `launch` returns: started on a pool thread after
        // a 50 ms head start, the collector could miss the refusal, which a
        // SharedFlow keeps for no one.
        val job = launch(Dispatchers.IO, start = CoroutineStart.UNDISPATCHED) { db.replica!!.refusals.collect { refused.add(it) } }
        // Offline, nothing is sent, so the row is read before the server
        // can refuse it: online, the 409 came back and the row was put back
        // before the test read it, on a loaded runner ([one, two]).
        offline(db)
        // The server's key is @unique; the replica's a plain hash.
        db.from("tasks").insert(mapOf("key" to "a", "title" to "dup", "status" to "open"))
        assertEquals(listOf("dup", "one", "two"), titles(db))
        db.replica!!.setOnline(true)
        eventually("the refusal") { refused.isNotEmpty() }
        assertEquals(409, refused[0].status)
        assertEquals(null, refused[0].quota)
        eventually("the write put back") { titles(db) == listOf("one", "two") }
        assertEquals(409, db.replica!!.refresh().error?.status)
        assertEquals(0, db.replica!!.refresh().pending)
        job.cancel()
        db.close()
    }

    /**
     * A require's reason goes with its write, and comes back with the refusal
     * that puts it back; one the replica finds unmet says it at once, nothing sent.
     */
    @Test
    fun aRequiresReasonComesBackWithItsRefusal() = runBlocking {
        val s = server("reason")
        val db = Fenec.sync(url = s.url, shapes = listOf(open), path = path("reason"))
        db.replica!!.ready()
        val refused = Collections.synchronizedList(ArrayList<Refusal>())
        val job = launch(Dispatchers.IO, start = CoroutineStart.UNDISPATCHED) { db.replica!!.refusals.collect { refused.add(it) } }
        val local = assertFailsWith<FenecException> {
            db.from("tasks").where("key", "zzz").delete(require = 1, reason = "no such task")
        }
        assertEquals(FenecException.Code.UNMET, local.code, local.message)
        assertEquals("no such task", local.reason)
        // Offline, the server's row goes while the replica still holds it:
        // the write's count is met here and not there, whichever of the
        // stream and the write the server meets first.
        offline(db)
        s.run("""del tasks where key = "a"""")
        db.from("tasks").where("key", "a").update(mapOf("title" to "X"), require = 1, reason = "the task was taken")
        db.replica!!.setOnline(true)
        eventually("the refusal") { refused.isNotEmpty() }
        assertEquals(412, refused[0].status)
        assertEquals("the task was taken", refused[0].reason)
        eventually("the write put back") { titles(db) == listOf("two") }
        job.cancel()
        db.close()
    }

    @Test
    fun aWriteATenantsQuotaRefusesIsPutBackAndTheDeleteBehindItGoes() = runBlocking {
        // A tenant node holding each tenant to the three rows it has.
        val s = Server("quota", tenantRows = 3).also { servers.add(it) }
        val db = Fenec.sync(url = s.url, shapes = listOf(open), path = path("quota"))
        db.replica!!.ready()
        val refused = Collections.synchronizedList(ArrayList<Refusal>())
        val job = launch(Dispatchers.IO, start = CoroutineStart.UNDISPATCHED) { db.replica!!.refusals.collect { refused.add(it) } }
        // Offline, so both are made before the server answers either.
        offline(db)
        db.from("tasks").insert(mapOf("key" to "d", "title" to "four", "status" to "open"))
        db.from("tasks").where("key", "b").delete()
        assertEquals(listOf("four", "one"), titles(db))
        db.replica!!.setOnline(true)
        eventually("the refusal") { refused.isNotEmpty() }
        assertEquals(507, refused[0].status)
        assertEquals("rows", refused[0].quota)
        // The insert put back, and the delete behind it landed.
        eventually("the queue answered") { db.replica!!.refresh().pending == 0 }
        assertEquals(listOf("one"), titles(db))
        assertEquals("rows", db.replica!!.refresh().error?.quota)
        assertTrue(s.run("get tasks where key = \"b\"").rows.isEmpty())
        job.cancel()
        db.close()
    }

    @Test
    fun aServerKilledAndStartedAgainIsCaughtUpWith() = runBlocking {
        val s = server("restart")
        val db = Fenec.sync(url = s.url, shapes = listOf(open), path = path("restart"))
        db.replica!!.ready()
        s.kill()
        eventually("offline") { db.replica!!.status.value.state == SyncStatus.State.OFFLINE }
        // A write while it is down waits in the file.
        db.from("tasks").insert(mapOf("title" to "while down", "status" to "open"))
        assertEquals(1, db.replica!!.refresh().pending)
        s.restart()
        s.run("""put tasks {key: "e", title: "after", status: "open"}""")
        db.replica!!.resume()
        eventually("caught up both ways") {
            titles(db) == listOf("after", "one", "two", "while down") && db.replica!!.refresh().pending == 0
        }
        assertEquals(1, s.run("""get tasks where title = "while down"""").rows.size)
        db.close()
    }

    @Test
    fun pendingWritesOutliveAReopen() = runBlocking {
        val s = server("reopen")
        val path = path("reopen")
        var db = Fenec.sync(url = s.url, shapes = listOf(open), path = path)
        db.replica!!.ready()
        s.kill()
        db.from("tasks").insert(mapOf("title" to "kept", "status" to "open"))
        db.from("tasks").where("key", "a").update(mapOf("title" to "ONE"))
        assertEquals(2, db.replica!!.refresh().pending)
        db.close()

        db = Fenec.sync(url = s.url, shapes = listOf(open), path = path)
        assertEquals(2, db.replica!!.refresh().pending)
        assertEquals(listOf("ONE", "kept", "two"), titles(db))
        s.restart()
        db.replica!!.resume()
        eventually("the queue sent") { db.replica!!.refresh().pending == 0 }
        assertEquals(1, s.run("""get tasks where title = "kept"""").rows.size)
        assertEquals(1, s.run("""get tasks where title = "ONE"""").rows.size)
        eventually("the server's copies") { db.from("tasks").rows().all { it.long("id")!! < 1L shl 52 } }
        db.close()
    }

    /**
     * The Kotlin tab's "over HTTP" lines on languages.html, as written there but for the server's address,
     * and the read the home page's Kotlin session ends with.
     */
    @Test
    fun theDocsExampleOverHttp() = runBlocking {
        val s = server("docs")
        s.run("create collection if not exists docs (title text @text, year int @sorted)")
        s.run("""put docs {title: "Night at the oasis", year: 2024}""")

        val remote = Fenec.connect(s.url, token = "secret")
        remote.execute("put docs {title: \$1, year: \$2}", "Dunes", 2021)
        val recent = remote.from("docs").select("title", "year").where("year", ">=", 2020).order("year", "desc").limit(5).rows()
        assertEquals(listOf("Night at the oasis", "Dunes"), recent.map { it.string("title") })

        val rows = remote.query("get docs select title, year where year >= \$1 order year desc limit \$2", 2020, 5)
        assertEquals(listOf(2024, 2021), rows.map { it.int("year") })
        assertEquals(recent.map { it.string("title") }, rows.map { it.string("title") })
    }

    @Test
    fun connectRunsEveryQueryOnTheServer() = runBlocking {
        val s = server("connect")
        val db = Fenec.connect(s.url)
        val rows = db.from("tasks").where("status", "open").order("priority", "desc").rows()
        assertEquals(listOf("two", "one"), rows.map { it.string("title") })
        assertEquals(1L, db.from("tasks").insert(mapOf("key" to "z", "title" to "remote")))
        assertEquals(4L, db.from("tasks").count())
        val e = assertFailsWith<FenecException> { db.from("tasks").insert(mapOf("key" to "z", "title" to "again")) }
        assertEquals(FenecException.Code.DUPLICATE, e.code)
    }

    /** A batch lands whole or not at all, says which statement stopped it, and under an idempotency key lands once. */
    @Test
    fun connectBatchesAndKeysWrites() = runBlocking {
        val s = server("batch")
        val db = Fenec.connect(s.url)
        val tasks = db.from("tasks")
        val out = db.batch(
            listOf(
                tasks.toInsert(mapOf("key" to "d", "title" to "four")),
                tasks.where("key", "a").toUpdate(mapOf("priority" to 9), require = 1),
                tasks.where("priority", ">=", 5).order("priority").select("key").toFenecQL(),
            ),
        )
        assertEquals(3, out.results.size)
        assertEquals(1L, out.results[0].affected)
        assertEquals(listOf("b", "a"), out.results[2].rows.map { it.string("key") })
        assertTrue(out.seq != null && out.seq == db.seq && !out.replayed)

        val e = assertFailsWith<FenecException> {
            db.batch(
                listOf(
                    tasks.toInsert(mapOf("key" to "e", "title" to "five")),
                    tasks.where("key", "nobody").toDelete(require = 1, reason = "nobody there"),
                ),
            )
        }
        assertEquals(listOf<Any?>(FenecException.Code.UNMET, 412, 1, 0), listOf(e.code, e.status, e.at, e.completed))
        // The statement's own words, which stay put where `at` moves.
        assertEquals("nobody there", e.reason)
        assertEquals(4L, tasks.count())

        val keyed = db.withIdempotencyKey("batch-1")
        val stmts = listOf(tasks.toInsert(mapOf("key" to "f", "title" to "six")))
        assertEquals(false, keyed.batch(stmts).replayed)
        assertEquals(true, keyed.batch(stmts).replayed)
        assertEquals(5L, tasks.count())
        db.runList("""put tasks {key: "h"}""", emptyList(), idempotencyKey = "put-1")
        db.runList("""put tasks {key: "h"}""", emptyList(), idempotencyKey = "put-1")
        assertEquals(6L, tasks.count())
        val reused = assertFailsWith<FenecException> {
            db.runList("""put tasks {key: "i"}""", emptyList(), idempotencyKey = "put-1")
        }
        assertEquals(listOf<Any?>(422, null), listOf(reused.status, reused.at))
    }

    /** A claim held at the server until a job comes ([FenecRemote.withWait]): taken once one is enqueued, ended on time with no row, and given up by cancelling its coroutine. */
    @Test
    fun connectHoldsAClaimUntilAJobComes() = runBlocking {
        val s = server("held")
        s.run("create collection jobs (kind text, run_at timestamp @sorted, owner text)")
        val db = Fenec.connect(s.url)
        suspend fun claim(c: FenecRemote, owner: String) = c.from("jobs").where(Cond.raw("run_at <= now()"))
            .order("run_at").limit(1)
            .updateReturning(mapOf("owner" to owner, "run_at" to Computed.expr("now() + ?", 60000)), returning = listOf("kind"))
        val held = async(Dispatchers.Default) { claim(db.withWait(10_000), "w1") }
        delay(200)
        db.execute("put jobs {kind: \"mail\", run_at: $1}", System.currentTimeMillis())
        assertEquals(listOf("mail"), withTimeout(5_000) { held.await() }.map { it.string("kind") })
        val began = System.nanoTime()
        assertTrue(claim(db.withWait(300), "w2").isEmpty())
        assertTrue(System.nanoTime() - began >= 290_000_000L)
        val given = async(Dispatchers.Default) { claim(db.withWait(10_000), "w3") }
        delay(100)
        val t = System.nanoTime()
        given.cancelAndJoin()
        assertTrue(System.nanoTime() - t < 2_000_000_000L, "a cancelled claim let go at once")
    }

    /** `/query` answers `{"rows": [...], "facets": {...}}` when the query asked facets, the bare array otherwise. */
    @Test
    fun highlightsAndFacetsOverHttp() = runBlocking {
        val s = server("facets")
        s.run("create collection docs (body text @text, kind text)")
        s.run("""put docs [{body: "rust is fast", kind: "lang"}, {body: "go is simple", kind: "lang"}, {body: "rust never sleeps", kind: "song"}]""")
        val db = Fenec.connect(s.url)
        val marked = db.from("docs").select("body").highlight("body").match("body", "rust").rows()
        assertEquals(2, marked.size)
        for (r in marked) assertEquals(listOf(listOf(0L, 4L)), r["highlight(body)"])
        val a = db.from("docs").where("kind", "!=", "none").facet("kind", top = 1).limit(1).answer()
        assertEquals(1, a.rows.size)
        assertEquals(mapOf("kind" to listOf(FacetCount("lang", 2))), a.facets)
        assertEquals(3, db.from("tasks").rows().size)
        assertEquals(emptyMap(), db.from("tasks").answer().facets)
    }

    @Test
    fun statusFlowsAsAStateFlow()= runBlocking {
        val s = server("status")
        val db = Fenec.sync(url = s.url, shapes = listOf(open), path = path("status"))
        withTimeout(10_000) { db.replica!!.status.first { it.state == SyncStatus.State.ONLINE } }
        db.close()
    }
}
