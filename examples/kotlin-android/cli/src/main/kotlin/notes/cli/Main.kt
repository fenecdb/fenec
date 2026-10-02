package notes.cli

import com.fenecdb.Fenec
import com.fenecdb.Row
import com.fenecdb.live
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import notes.Notes
import java.nio.file.Files
import kotlin.system.exitProcess

const val USAGE = """usage: notes                       seeds if empty, then lists
       notes add <title> <body> [tag ...]
       notes list [--tag T] [--open]
       notes search <words>        match, then fuse with the toy embedding
       notes done <id>
       notes watch                 the open notes again after every change (Ctrl-C)
       notes smoke                 the whole tour on a new file, checked"""

fun main(args: Array<String>) = runBlocking {
    val cmd = args.firstOrNull() ?: "list"
    if (cmd == "smoke") return@runBlocking smoke()
    val db = Notes.open(System.getenv("NOTES_FILE") ?: "notes.fenec")
    db.use {
        when (cmd) {
            "add" -> {
                if (args.size < 3) usage()
                Notes.add(db, args[1], args[2], args.drop(3))
                show(Notes.list(db).rows())
            }
            "list" -> {
                val tag = args.indexOf("--tag").takeIf { it > 0 }?.let { args.getOrNull(it + 1) ?: usage() }
                var q = if (tag != null) Notes.tagged(db, tag) else Notes.list(db)
                if ("--open" in args) q = q.where("done", false)
                show(q.rows())
            }
            "search" -> {
                val words = args.drop(1).joinToString(" ").ifEmpty { usage() }
                println("match:")
                show(Notes.search(db, words).rows())
                println("fuse (match + toy embedding):")
                show(Notes.hybrid(db, words).rows())
            }
            "done" -> {
                Notes.done(db, args.getOrNull(1)?.toLongOrNull() ?: usage())
                show(Notes.pending(db).rows())
            }
            "watch" -> db.live(Notes.pending(db)).collect { rows ->
                println("-- open notes (${rows.size})")
                show(rows)
            }
            else -> usage()
        }
    }
}

fun usage(): Nothing {
    System.err.println(USAGE)
    exitProcess(2)
}

fun show(rows: List<Row>) {
    for (r in rows) {
        val mark = if (r.bool("done") == true) "x" else " "
        println("[$mark] ${r.long("id")}  ${r.string("title")}  ${r.list("tags")?.joinToString(",")}  ${r.string("at")}")
    }
}

fun check(step: String, ok: Boolean, got: Any?) {
    if (!ok) {
        System.err.println("FAIL $step: got $got")
        exitProcess(1)
    }
    println("ok   $step")
}

/** The tour CI runs: every query of the app on a new file, then the file opened again. */
suspend fun smoke() = kotlinx.coroutines.coroutineScope {
    val path = Files.createTempDirectory("notes").resolve("notes.fenec").toString()
    val db = Notes.open(path)
    check("seeded", db.from("notes").count() == 4L, db.from("notes").count())

    val hello = Notes.embed("hello").withIndex().filter { it.value != 0f }.map { it.index }
    check("toy embedding", hello == listOf(24, 36, 46, 48, 62), hello)

    val titles = { rows: List<Row> -> rows.map { it.string("title") } }
    val list = titles(Notes.list(db).rows())
    check("newest first", list.first() == "Book club", list)
    val work = titles(Notes.tagged(db, "work").rows())
    check("tag work", work == listOf("Book flights", "Release checklist"), work)
    val open = Notes.pending(db).rows()
    check("open", open.size == 3, titles(open))
    val match = titles(Notes.search(db, "release docs").rows())
    check("match", match.first() == "Release checklist", match)
    val near = titles(Notes.similar(db, "flights to Istanbul").rows())
    check("near", near.first() == "Book flights", near)
    val fuse = titles(Notes.hybrid(db, "desert fox").rows())
    check("fuse", fuse.first() == "Book club", fuse)

    // A live query hands over its rows now, and again after the insert.
    val seen = Channel<List<Row>>(Channel.CONFLATED)
    val watching = launch { db.live(Notes.pending(db)).collect { seen.send(it) } }
    withTimeout(5_000) { while (seen.receive().size != 3) Unit }
    Notes.add(db, "Call mom", "Ask about the weekend.", listOf("home"))
    withTimeout(5_000) { while (seen.receive().size != 4) Unit }
    check("live", true, null)
    watching.cancel()

    val groceries = db.from("notes").where("title", "Groceries").first()!!.long("id")!!
    Notes.done(db, groceries)
    val left = db.from("notes").where("done", false).count()
    check("done", left == 3L, left)

    db.close()
    Notes.open(path).use { again ->
        check("persisted", again.from("notes").count() == 5L, again.from("notes").count())
    }
}
