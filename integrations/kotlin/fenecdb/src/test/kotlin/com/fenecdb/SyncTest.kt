package com.fenecdb

import kotlinx.coroutines.Dispatchers
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
class Server(name: String) {
    private val dir: File = Files.createTempDirectory("fenecdb-kotlin-$name").toFile()
    private val file = File(dir, "server.fenec").path
    private var process: Process? = null
    var port = 0
        private set
    val url get() = "http://127.0.0.1:$port"

    init {
        start(0)
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
        val p = ProcessBuilder(binary, "--http", "127.0.0.1:$port", "--file", file, "--sync", "always")
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
        db.from("tasks").insert(mapOf("title" to "new", "status" to "open", "priority" to 2))
        // At once, under a temporary id.
        assertTrue((db.from("tasks").where("title", "new").first()!!.long("id") ?: 0) >= 1L shl 52)
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
        val job = launch(Dispatchers.IO) { db.replica!!.refusals.collect { refused.add(it) } }
        delay(50)
        // The server's key is @unique; the replica's a plain hash.
        db.from("tasks").insert(mapOf("key" to "a", "title" to "dup", "status" to "open"))
        assertEquals(listOf("dup", "one", "two"), titles(db))
        eventually("the refusal") { refused.isNotEmpty() }
        assertEquals(409, refused[0].status)
        eventually("the write put back") { titles(db) == listOf("one", "two") }
        assertEquals(409, db.replica!!.refresh().error?.status)
        assertEquals(0, db.replica!!.refresh().pending)
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

    @Test
    fun statusFlowsAsAStateFlow() = runBlocking {
        val s = server("status")
        val db = Fenec.sync(url = s.url, shapes = listOf(open), path = path("status"))
        withTimeout(10_000) { db.replica!!.status.first { it.state == SyncStatus.State.ONLINE } }
        db.close()
    }
}
