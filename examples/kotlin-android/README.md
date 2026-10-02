# Notes -- Kotlin (Android and the JVM)

The Notes app with fenecdb embedded: the database is a file on the device,
no server. Two front ends over one module of logic:

- `app/` -- a Jetpack Compose app, one screen, with `com.fenecdb:fenecdb-android`;
- `cli/` -- the same app as a JVM command line, with `com.fenecdb:fenecdb`;
- `notes/` -- what both share: the schema, the seed notes, the queries and
  the toy embedding.

## What it shows

- the schema as FenecQL (`notes/src/main/resources/schema.fenecql`), applied
  with `db.schema(...)` after every open;
- creating notes with the query builder;
- full-text search with `match`, and `match` and `near` fused (`fuse`);
- filtering by tag (`tags has ...`) and by `done`, newest first (`order at desc`);
- a live query: a `Flow` that hands the screen its rows again after every write;
- persistence: the file in `context.filesDir` (the CLI: `notes.fenec` in the
  current directory) holds everything across restarts.

## Prerequisites

- JDK 17 and Gradle 8.7 or newer;
- for the app, the Android SDK (API 34) -- Android Studio opens this
  directory as a project;
- for the CLI, the native library for your machine: the AAR carries it on
  Android, the JVM needs `libfenec_ffi` built with its JNI functions. From
  a clone of the repository: `make ffi JNI=1`, which prints the path.

## Run

The app, on a device or an emulator:

```sh
cd examples/kotlin-android
gradle :app:installDebug
```

The CLI:

```sh
cd examples/kotlin-android
export FENEC_LIBRARY=/path/to/libfenec_ffi.so     # .dylib on macOS
gradle -q :cli:run                                  # seeds, then lists
gradle -q :cli:run --args='add "Dentist" "Call to move the appointment." home'
gradle -q :cli:run --args='list --tag work'
gradle -q :cli:run --args='list --open'
gradle -q :cli:run --args='search release docs'
gradle -q :cli:run --args='done 1'
gradle -q :cli:run --args=watch                     # live: the open notes after every change
gradle -q :cli:run --args=smoke                     # the whole tour, checked
```

`NOTES_FILE` names another file. A file is opened by one process at a time:
`watch` holds it, so stop it before adding from another terminal, or add
from the app.

## The core

```kotlin
val db = Fenec.openAsync(File(context.filesDir, "notes.fenec").path)
db.schema(schemaText)                                  // makes what is missing
db.from("notes").insert(mapOf("title" to t, "body" to b, "tags" to tags,
    "done" to false, "at" to Date(), "embed" to Notes.embed("$t $b")))
val open = db.from("notes").where("done", false).order("at", "desc")
val rows by remember(open) { db.live(open) }.collectAsState(emptyList())   // Compose
db.from("notes").match("body", words).rows()                               // BM25
db.from("notes").match("body", words).near("embed", Notes.embed(words)).fuse().rows()
db.from("notes").where("tags", "has", "work").rows()
db.from("notes").where("id", id).update(mapOf("done" to true))
```

## The toy embedding

`Notes.embed` is a placeholder for a real model: it hashes the text's
character trigrams into 64 dimensions, enough for `near` and `fuse` to show
what they do and the same in every example of this repository. A real app
calls an embeddings API there, or runs a model on the device (ONNX Runtime
or TensorFlow Lite), and sets `vector<N>` in `schema.fenecql` to its size.

## Against this repository's build

`FENEC_LOCAL=1` builds the library from `integrations/kotlin` (a Gradle
composite build) instead of taking it from Maven Central, which is how CI
runs it:

```sh
examples/kotlin-android/run-smoke.sh
```

On Linux with Gradle and Java it builds the native library with Cargo and
runs `:cli:run --args=smoke` there, then `:app:assembleDebug` where an
Android SDK is installed; elsewhere the CLI runs in `rust` and `gradle`
containers (Docker).
