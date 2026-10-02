package com.fenecdb.notes

import android.content.Context
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Button
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.unit.dp
import com.fenecdb.Fenec
import com.fenecdb.live
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import notes.Notes
import java.io.File

/** One database for the process: a file is opened once, whatever the activity does. */
object Store {
    private val lock = Mutex()
    private var db: Fenec? = null

    suspend fun db(context: Context): Fenec = lock.withLock {
        db ?: Notes.open(File(context.filesDir, "notes.fenec").path).also { db = it }
    }
}

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            MaterialTheme {
                Surface(Modifier.fillMaxSize()) {
                    var db by remember { mutableStateOf<Fenec?>(null) }
                    LaunchedEffect(Unit) { db = Store.db(applicationContext) }
                    db?.let { NotesScreen(it) } ?: Text("Opening...", Modifier.padding(16.dp))
                }
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun NotesScreen(db: Fenec) {
    val scope = rememberCoroutineScope()
    var words by remember { mutableStateOf("") }
    var tag by remember { mutableStateOf<String?>(null) }
    var openOnly by remember { mutableStateOf(false) }
    var title by remember { mutableStateOf("") }
    var body by remember { mutableStateOf("") }

    // Words rank by match and the toy embedding fused; otherwise newest first.
    val query = remember(words, tag, openOnly) {
        var q = if (words.isBlank()) Notes.list(db) else Notes.hybrid(db, words)
        tag?.let { q = q.where("tags", "has", it) }
        if (openOnly) q = q.where("done", false)
        q
    }
    // A live query: its rows again after every write to notes.
    val rows by remember(query) { db.live(query) }.collectAsState(emptyList())

    Column(Modifier.fillMaxSize().padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
        OutlinedTextField(words, { words = it }, Modifier.fillMaxWidth(), label = { Text("Search") }, singleLine = true)
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            for (t in listOf("home", "work", "travel")) {
                FilterChip(selected = tag == t, onClick = { tag = if (tag == t) null else t }, label = { Text(t) })
            }
            FilterChip(selected = openOnly, onClick = { openOnly = !openOnly }, label = { Text("open") })
        }
        OutlinedTextField(title, { title = it }, Modifier.fillMaxWidth(), label = { Text("Title") }, singleLine = true)
        OutlinedTextField(body, { body = it }, Modifier.fillMaxWidth(), label = { Text("Note") })
        Button(
            enabled = title.isNotBlank(),
            onClick = {
                val (t, b) = title.trim() to body.trim()
                title = ""
                body = ""
                scope.launch { Notes.add(db, t, b, tag?.let(::listOf) ?: emptyList()) }
            },
        ) { Text("Add") }
        LazyColumn(verticalArrangement = Arrangement.spacedBy(12.dp)) {
            items(rows, key = { it.long("id")!! }) { note ->
                val done = note.bool("done") == true
                Column(Modifier.fillMaxWidth().clickable { scope.launch { Notes.done(db, note.long("id")!!) } }) {
                    Text(
                        note.string("title") ?: "",
                        fontWeight = FontWeight.Bold,
                        textDecoration = if (done) TextDecoration.LineThrough else null,
                    )
                    Text(note.string("body") ?: "", style = MaterialTheme.typography.bodyMedium)
                    Text(
                        (note.list("tags") ?: emptyList()).joinToString("  ") { "#$it" },
                        style = MaterialTheme.typography.labelSmall,
                    )
                }
            }
        }
    }
}
