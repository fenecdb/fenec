# fenecdb for Kotlin and Java

fenecdb embedded in an Android or JVM app: the database is a file on the
device, the engine the native library (`crates/fenec-ffi`) reached through
JNI functions it carries itself -- on its own, or a replica a server keeps
in step.

```kotlin
// build.gradle.kts
implementation("com.fenecdb:fenecdb-android:0.1.7")   // Android: the AAR, the library for each ABI inside
implementation("com.fenecdb:fenecdb:0.1.7")           // the JVM: bring libfenec_ffi for the platform
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
write to a collection it reads, once a burst. To see the text and
parameters a chain builds, for logging or a test, `query.toFenecQL()`
returns them and runs nothing. Every write is fsynced before
it returns unless the file is opened with `Fenec.NO_SYNC`, which leaves the
writes for `sync()`; `flush()` hands them to the system, which outlives the
app being killed -- what `onStop` calls. The docs:
`site/content/docs/mobile.html`.

Rows are `Row`s, a `Map<String, Any?>` in the answer's order with typed
getters (`string`, `long`, `double`, `bool`, `floats`, `row`, `list`); JSON is
read by a reader of the library's own, so it depends on kotlinx-coroutines
alone.

## Sync with a server

```kotlin
val db = Fenec.sync(
    url = "https://api.example.com", token = jwt,
    shapes = listOf(Shape("todos", where = mapOf("owner" to me), key = "key")),
    path = File(context.filesDir, "todos.fenec").path,
    tokenProvider = { refreshToken() },
)
db.replica!!.ready()
```

The same `Fenec`: reads and live queries are the file's, a write to a
shape's collection shows at once and is sent -- queued in the file while
the server cannot be reached, under an idempotency key so it lands once --
and one the server refuses is put back and comes on `replica.refusals`.
`replica.status` is a `StateFlow`: `ONLINE`, `OFFLINE` or `CATCHING_UP`,
the writes not yet answered and the last error; call `replica.resume()` in
`onStart`. The requests and the stream are `HttpURLConnection`'s, the JVM's
and Android's own, so a server is reached through TLS (Android refuses
cleartext by default). `Fenec.connect(url, token)` is a server with no
file: every query a request, the same builder.

## Building and testing

`make kotlin-test` builds the native library for Linux with its JNI
functions and runs the JVM tests under Gradle -- the engine, the builder
over every case of `integrations/builder-golden.json`, live queries, a
replica against a `fenec-server` the tests start, kill and start again --
in containers unless this is Linux with Gradle and Java.
`integrations/kotlin/build-aar.sh` builds the AAR with the NDK for
arm64-v8a, armeabi-v7a and x86_64 (`ANDROID_HOME`, `ANDROID_NDK_HOME`).
