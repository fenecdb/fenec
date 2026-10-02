# fenecdb for Kotlin and Java

fenecdb embedded in an Android or JVM app: the database is a file on the
device, the engine the native library (`crates/fenec-ffi`) reached through
JNI functions it carries itself. No server, no network.

```kotlin
// build.gradle.kts
implementation("io.github.fenecdb:fenecdb-android:0.1.7")   // Android: the AAR, the library for each ABI inside
implementation("io.github.fenecdb:fenecdb:0.1.7")           // the JVM: bring libfenec_ffi for the platform
```

```kotlin
val db = Fenec.openAsync(File(context.filesDir, "app.fenec").path)
db.execute("create collection todos (title text, done bool @hash)")
db.from("todos").insert(mapOf("title" to "milk", "done" to false))
val open = db.from("todos").where("done", false).rows()
val near = db.query("get notes near embed \$1 limit 5", embedding)   // a FloatArray

@Composable
fun Todos(db: Fenec) {
    val todos by remember { db.live(db.from("todos").where("done", false)) }.collectAsState(emptyList())
    LazyColumn { items(todos) { Text(it.string("title") ?: "") } }
}
```

The suspending calls run on `Dispatchers.IO`; Java calls the `...Blocking`
ones (`queryBlocking`, `executeBlocking`, `rowsBlocking` ...) off its main
thread. A live query is a conflated `Flow<List<Row>>`, run again after a
write to a collection it reads, once a burst. Every write is fsynced before
it returns unless the file is opened with `Fenec.NO_SYNC`, which leaves the
writes for `sync()`; `flush()` hands them to the system, which outlives the
app being killed -- what `onStop` calls. The docs:
`site/content/docs/mobile.html`.

Rows are `Row`s, a `Map<String, Any?>` in the answer's order with typed
getters (`string`, `long`, `double`, `bool`, `floats`, `row`, `list`); JSON is
read by a reader of the library's own, so it depends on kotlinx-coroutines
alone.

## Building and testing

`make kotlin-test` builds the native library for Linux with its JNI
functions and runs the JVM tests under Gradle -- the engine, the builder
over every case of `integrations/builder-golden.json`, live queries -- in
containers unless this is Linux with Gradle and Java.
`integrations/kotlin/build-aar.sh` builds the AAR with the NDK for
arm64-v8a, armeabi-v7a and x86_64 (`ANDROID_HOME`, `ANDROID_NDK_HOME`).
