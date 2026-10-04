package com.fenecdb

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.awaitAll
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.catch
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import java.util.Collections
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

class LiveTest {
    private suspend fun todos(): Fenec {
        val db = Fenec.memory()
        db.execute("create collection todos (title text, done bool @hash); create collection other (n int)")
        db.from("todos").insert(listOf(mapOf("title" to "milk", "done" to false), mapOf("title" to "bread", "done" to true)))
        return db
    }

    /** Waits until [done] holds, or fails after a few seconds. */
    private suspend fun until(what: String, done: () -> Boolean) {
        withTimeout(5_000) { while (!done()) delay(10) }
        assertTrue(done(), what)
    }

    @Test
    fun aFlowHasItsRowsNowAndAfterEachWrite() = runBlocking {
        val db = todos()
        val seen = Collections.synchronizedList(ArrayList<List<String?>>())
        val job = launch(Dispatchers.IO) {
            db.live(db.from("todos").select("title").where("done", false)).collect { rows -> seen.add(rows.map { it.string("title") }) }
        }
        until("the first rows") { seen.size == 1 }
        assertEquals(listOf("milk"), seen[0])
        db.from("todos").insert(mapOf("title" to "eggs", "done" to false))
        until("the rows after a put") { seen.size == 2 }
        assertEquals(listOf("milk", "eggs"), seen[1])
        // A write to a collection it does not read runs nothing.
        db.execute("put other {n: 1}")
        delay(80)
        assertEquals(2, seen.size)
        db.from("todos").where("title", "milk").update(mapOf("done" to true))
        until("the rows after a set") { seen.size == 3 }
        assertEquals(listOf("eggs"), seen[2])
        job.cancel()
        db.close()
    }

    /** A live query's rows carry what its `facet` counted, over every row and not the page alone. */
    @Test
    fun aLiveQueryIsHandedItsFacets() = runBlocking {
        val db = todos()
        val seen = Collections.synchronizedList(ArrayList<Rows>())
        val job = launch(Dispatchers.IO) { db.live(db.from("todos").facet("done").limit(1)).collect { seen.add(it) } }
        until("the first rows") { seen.size == 1 }
        assertEquals(1, seen[0].size)
        assertEquals(setOf(FacetCount(false, 1), FacetCount(true, 1)), seen[0].facets["done"]!!.toSet())
        db.from("todos").insert(mapOf("title" to "eggs", "done" to false))
        until("the rows after a put") { seen.size == 2 }
        assertEquals(listOf(FacetCount(false, 2), FacetCount(true, 1)), seen[1].facets["done"])
        job.cancel()
        db.close()
    }

    /** The writes of a burst -- several at once, and a text of several statements -- run the query once. */
    @Test
    fun aBurstOfWritesRunsItOnce() = runBlocking {
        val db = todos()
        val all = db.from("todos")
        val runs = Collections.synchronizedList(ArrayList<Int>())
        val job = launch(Dispatchers.IO) { db.live(all).collect { runs.add(it.size) } }
        until("the first rows") { runs.size == 1 }
        // Several at once are calls that overlap, and a call held under way
        // across the burst makes them so. Launched on the IO pool alone they
        // overlapped only as the scheduler had them: on a loaded runner one
        // insert ended, and its look ran a frame later, before the rest
        // began -- the query ran three times ([2, 10, 12]).
        db.inflight.incrementAndGet()
        try {
            (0 until 10).map { i -> async(Dispatchers.IO) { all.insert(mapOf("title" to "t$i", "done" to false)) } }.awaitAll()
            // Nothing runs while a write is under way.
            delay(100)
            assertEquals(listOf(2), runs.toList())
        } finally {
            // The held call ending, as `answerBlocking` ends one.
            db.inflight.decrementAndGet()
            db.lives.touch()
        }
        until("the rows after the burst") { runs.lastOrNull() == 12 }
        delay(100)
        assertEquals(listOf(2, 12), runs.toList())
        db.execute("put todos {title: \"x\"}; put todos {title: \"y\"}; put other {n: 2}")
        until("the rows after a text") { runs.lastOrNull() == 14 }
        delay(100)
        assertEquals(3, runs.size)
        // A text that fails is put back whole and runs nothing.
        runCatching { db.execute("put todos {title: \"z\"}; put other {n: \"no\"}") }
        delay(100)
        assertEquals(3, runs.size)
        job.cancel()
        delay(20)
        all.insert(mapOf("title" to "after", "done" to false))
        delay(100)
        assertEquals(3, runs.size)
        db.close()
    }

    /** A text names the collections it reads, or runs after every write; a drop runs everything. */
    @Test
    fun aTextAndADrop() = runBlocking {
        val db = todos()
        val counts = Collections.synchronizedList(ArrayList<Long?>())
        val errors = Collections.synchronizedList(ArrayList<Throwable>())
        val scope = CoroutineScope(Dispatchers.IO)
        scope.launch { db.live("get todos count", collections = listOf("todos")).collect { counts.add(it.first().long("count")) } }
        scope.launch { db.live("get other count").catch { errors.add(it) }.collect {} }
        until("the first rows") { counts.size == 1 }
        db.execute("put todos {title: \"t\"}")
        until("the count") { counts.lastOrNull() == 3L }
        db.execute("drop collection other")
        until("the error") { errors.isNotEmpty() }
        assertEquals(FenecException.Code.NOT_FOUND, (errors[0] as FenecException).code)
        val refused = runCatching { db.live(db.from("nowhere")).first() }.exceptionOrNull()
        assertEquals(FenecException.Code.NOT_FOUND, (refused as FenecException).code)
        db.close()
    }
}
