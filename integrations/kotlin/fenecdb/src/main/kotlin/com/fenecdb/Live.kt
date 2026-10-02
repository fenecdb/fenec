package com.fenecdb

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.buffer
import kotlinx.coroutines.flow.callbackFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.atomic.AtomicBoolean

/**
 * The live queries over one database, as `web/fenec.js`'s `Lives` keeps a
 * page's: the change ring says which collections were written since a
 * cursor ([Fenec.changes]), a block's once it lands whole, so knowing costs
 * a write nothing -- it is asked once after a burst of writes, and only by
 * a database holding a live query. Collection granularity: a query that
 * reads what was written runs again from scratch, well under a millisecond
 * over a local file, where anything finer would cost more than the query.
 *
 * A write through the database asks for a look ([touch]); the looks a burst
 * asks for are one, taken a frame later and once no write is under way, so
 * a loop of writes or several at once run each query once.
 */
internal class Lives(private val db: Fenec) {
    class Sub(
        val rows: suspend () -> List<Row>,
        val reads: Set<String>?,
        val deliver: (List<Row>) -> Unit,
        val fail: (Throwable) -> Unit,
    ) {
        @Volatile var on = true
    }

    companion object {
        /** How long a burst's looks are gathered: a frame at 60 Hz, the soonest a screen shows anything. */
        const val GATHER_MS = 16L
    }

    private val subs = CopyOnWriteArrayList<Sub>()
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val due = AtomicBoolean(false)

    /** One look at a time, in order. */
    private val looking = Mutex()

    @Volatile private var cursor = 0L

    fun touch() {
        if (subs.isEmpty() || !due.compareAndSet(false, true)) return
        scope.launch {
            delay(GATHER_MS)
            due.set(false)
            // A write under way asks again as it ends.
            if (db.writing) return@launch
            looking.withLock { tick() }
        }
    }

    suspend fun add(sub: Sub) {
        looking.withLock {
            // With none before it no look has kept the cursor up: it starts
            // here, where the first run reads.
            if (subs.isEmpty()) cursor = db.changes(Long.MAX_VALUE).seq
            subs.add(sub)
            run(sub)
        }
    }

    fun remove(sub: Sub) {
        sub.on = false
        subs.remove(sub)
    }

    fun clear() {
        subs.forEach { it.on = false }
        subs.clear()
        scope.cancel()
    }

    private suspend fun tick() {
        val info = runCatching { db.changes(cursor) }.getOrNull() ?: return
        cursor = info.seq
        val dirty = info.collections?.toSet()
        // Only reads since: nothing to run.
        if (dirty != null && dirty.isEmpty()) return
        for (s in subs) {
            if (dirty != null && s.reads != null && s.reads.none { it in dirty }) continue
            run(s)
        }
    }

    private suspend fun run(s: Sub) {
        try {
            val rows = s.rows()
            // Stopped while it ran: its rows go nowhere.
            if (s.on) s.deliver(rows)
        } catch (e: Throwable) {
            if (s.on) s.fail(e)
        }
    }

    /**
     * The rows of [text] now, and again after every write to [reads] (or to
     * anything, for `null`), as a [Flow]: conflated, so a collector that
     * falls behind sees the latest rows, and an error ends it. Collected as
     * Compose state with `collectAsState(emptyList())`.
     */
    fun flow(text: String, params: List<Any?>, reads: Set<String>?, failure: Throwable?): Flow<List<Row>> =
        callbackFlow {
            val sub = Sub(
                rows = {
                    if (failure != null) throw failure
                    db.answerBlocking(text, params, quiet = true).rows
                },
                reads = reads,
                deliver = { trySend(it) },
                fail = { close(it) },
            )
            add(sub)
            awaitClose { remove(sub) }
        }.buffer(Channel.CONFLATED)
}

/**
 * A live query: the rows of [query] now, and again after every write to a
 * collection it reads -- the writes of a burst run it once.
 *
 * ```kotlin
 * val open by db.live(db.from("todos").where("done", false)).collectAsState(emptyList())
 * ```
 */
fun Fenec.live(query: Query): Flow<List<Row>> {
    val (text, params, failure) = try {
        val (t, p) = query.toFenecQL()
        Triple(t, p, null)
    } catch (e: FenecException) {
        // A chain the builder refuses is the live query's error, where its
        // rows would have been.
        Triple("", emptyList<Any?>(), e)
    }
    return lives.flow(text, params, query.reads?.toSet(), failure)
}

/**
 * A live FenecQL text: run again after every write to [collections], or to
 * anything when it names none.
 */
fun Fenec.live(text: String, vararg params: Any?, collections: List<String>? = null): Flow<List<Row>> =
    lives.flow(text, params.toList(), collections?.toSet(), null)
