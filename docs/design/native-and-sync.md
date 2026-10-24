# The native library and sync

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *The native library and sync*: `fenec-ffi`, the sans-IO sync core and the scenario file both sync runners are held to. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## The native library

**An app keeps its database through `fenec-ffi`.** The engine as a native
library -- a cdylib and a staticlib, zero dependencies, its header
`include/fenec.h` written by hand and held to the exports by a test -- for
the Swift (`integrations/swift`, `Package.swift` at the root since SwiftPM
fetches a package by its repository), Kotlin (`integrations/kotlin`) and
Dart/Flutter (`integrations/dart`) bindings -- Maven Central's
`com.fenecdb:fenecdb` and `com.fenecdb:fenecdb-android`, Kotlin package
`com.fenecdb`, the JNI symbols `Java_com_fenecdb_FenecNative_*` with it.
A text is answered through
`fenec-abi`, the code the browser module answers through too -- prepare,
the exact pass, the block, the answer's JSON, the change notice -- so a page
and an app get the same bytes; out of the module's crate the optimizer at
`opt-level = "z"` kept those functions out of line (+762 bytes), and
`#[inline(always)]` left the module 161 bytes larger and 216 smaller in
brotli. A handle is a number into a table of `Arc`s, never a pointer: a use
after close finds nothing, and a call holds its database while it runs. A
read takes the shared lock -- a long one only to pin what it reads, and
reads with none held, as a server's does (`Database::pin`; a text of
several reads is one pin) -- a write the exclusive one, a lone `create
index` or `compact` runs beside the database (`Database::maintain`), and a
write's fsync runs once the lock is let go (`Database::flush`'s
`Durability`, a failure to `Database::fail`), as a server's do. Every call
runs under `catch_unwind` and returns a code -- an `Error`'s kind 1-10, 14
(`FENEC_UNMET`) or 15 (`FENEC_QUOTA`), or `FENEC_PANIC`, `FENEC_MISUSE`,
`FENEC_LOCKED` -- with the error's JSON, so
the library is built in the `ffi` profile, which unwinds. `<file>.lock` is
held while a file is open (a checkpoint renames a new file over the
database, which a lock on it would not survive): a second open, here or in
an app extension, is refused -- by a record lock (`fcntl`'s `F_SETLK`), the
process's, and a list of this process's lock files. An `flock` was the open
file's, which a child spawned meanwhile shares until its exec: a close and
an open again beside a thread spawning children were refused 48-66 times
in 3 000, and once in CI's `swift test`, whose servers start beside the
other tests. Every write is fsynced unless
`FENEC_OPEN_NO_SYNC`; `fenec_flush` is `Database::write_out`, the buffer
written with no fsync; `fenec_close` saves a graph once the file grew three
times its record since its last save, and syncs. A handle opened so writes
its buffer behind the lock as a server does (`Native::after`, `behind`):
`Database::write_behind` is asked after every write no fsync follows --
what a sync applies from its server, on either kind of handle, among them
(a fsynced handle so asked then hands over at a block's start, as a server
written behind does) -- and what it hands back runs once the lock is let
go, a failure the call's error and `Database::fail`'s, so the writes stop. Written under the
lock as it outgrew a mebibyte, a read by id beside texts of 1 000 128-dim
rows waited 0.24-0.25 ms p50, 2.30-4.44 at p99 and 12.9-61.3 at most, read
267-296k times a second beside a writer's 640-705k rows; behind it,
0.14-0.19, 1.15-1.28 and 1.33-1.86, 341-354k reads and 678-717k rows
(`ffi_bench beside`, twelve runs a side in ABBA turns at loads of 1.8-2.5).
A fsynced handle's bytes were the durability's already, written with no
lock held: beside fsynced texts a read waited 0.13-0.15 ms, 1.19-1.36 and
1.22-1.68 either way (`beside durable`). What either still writes under the
lock is a record of a mebibyte or more, where it lies (a server's too), and
on a fsynced handle the record a handover lands with. A lone buffered put
is 2.71 us p50 either way over sixteen runs a side, the mean 3.17 against
3.21 inside the runs' 2.27-3.27 and 2.63-3.73 (`ffi_bench put`), and `make
ffi-bench`'s put beside a graph 4.4 us p50 either way over ten, `near` and
the open as they were: written behind, the buffer goes out in writes of 64
KB rather than of a mebibyte. `open_wrapped` takes a wrap for the file's
sink, at which `tests/behind.rs` holds the write behind or fails it, and
the library's code grew 512 bytes. The Kotlin binding's JNI
functions are the library's (feature `jni`), four entries of the JNI table
by hand, a call one `byte[]` -- its code, its JSON -- and text as UTF-8
bytes, since JNI's modified UTF-8 splits an emoji. Each binding runs its
calls off the main thread (a dispatch queue, `Dispatchers.IO`, a worker
isolate a database), sends a vector as its `f32` bytes and a json field's
`exact` refusal again as JSON, holds its builder to every golden case, and
has live queries as `Lives` has them, the looks of a burst gathered a frame
(16 ms) and taken once no write is under way. The libraries are built alone
(`cargo rustc --crate-type cdylib`): beside the staticlib and rlib, LTO left
the shared one 4% larger. 1.68 MB stripped on aarch64-apple-darwin, 1.88 on
x86_64 Linux with JNI, the sync (below) 165 and 198 KB of them; a buffered put of a 128-dim vector 4.6 us against
the server handler's 23.2, a fsynced one 4.0 ms on an M1, `near` 97.8 us
against 151.4 (`make ffi-bench`). The XCFramework is assembled by hand
(`build-xcframework.sh`), so the Command Line Tools build every slice; a
release's zip is built before its tag (`swift-binary.yml`), since the tag's
`Package.swift` must name its checksum. Swift links a static library; the
Flutter plugin vendors a dynamic `FenecFFI.xcframework` (`--dynamic`,
`FenecFFI.framework` a slice, install name `@rpath/FenecFFI.framework/...`)
and Dart opens `FenecFFI.framework/FenecFFI`: a static one kept whole with
`-force_load` had the Runner link a file CocoaPods' "Copy XCFrameworks"
phase makes with no order declared against it, and `flutter build ios`
failed on it.

## Sync core

**A replica's sync is a state machine; the network is the binding's.**
iOS (ATS) and Android refuse cleartext HTTP by default and a server sits
behind TLS, which fenecdb's own client does not speak and should not: the
platform's client has the trust store, proxies, pinning and power
management. So `fenec_abi::sync` does no I/O -- a binding feeds it events
(`Sync::response`, `opened`, `bytes`, `closed`, `timer`, `signal`; FFI
`fenec_sync_feed`) and performs the JSON actions it hands back (`request`,
`stream`, `cancel`, `wait`, `token`, `changed`, `refused`, `status`) with
`URLSession`, `HttpURLConnection` (Android's and the JVM's, one loop for
both) or `dart:io`'s `HttpClient`, every event handed to the core in turn on
one serial queue so a stream's bytes stay in order and no action outruns
its cancel. Porting `FenecSync` would have been three more copies of the
optimistic writes, the key reconciliation and the cursors, tested apiece;
here the logic is once, held to the scenario file (below), and each
binding's loop is a page. A write
through `fenec_query` to a shape's collection is the sync's
(`Sync::claims`, `write`): applied to the replica with what puts it back,
and queued with an `Idempotency-Key`, in one block; DDL over a synced
collection is refused, a collection with no shape is the app's own. What
`FenecSync` holds in memory is in the file, written in the blocks it
describes: `_sync_shapes` (cursors), `_sync_queue` (unanswered writes, their
keys and undo), `_sync_temps` (rows under temporary ids). Writes go one at
a time in order; 0/408/429/5xx retry with backoff (250 ms doubling to 15 s,
30% jitter) or the `retry_after` a quota names where longer, 401 asks for
a token, a 507 naming a tenant's quota and any other 4xx are a refusal put
back (below). A
replica reopened sends its queue before opening its streams. An update of
an unanswered insert reaches the server's copy by its key as well
(`fenec_ql::spans` keeps each statement's own text for the `/batch` lines,
an insert rendered as the builders render it, every value a parameter); an
insert the shape does not hold loses its temporary row once a stream passes
the write's `Fenec-Seq`; a seed writes over and deletes the rest, keeping
unanswered writes' rows, rather than clearing first (a row that stayed
keeps its vector's node). A change with `"schema": true`, or rows naming a
field the replica has not got, has the collection made again from `GET
/collections`, its rows kept in the fields left (`remake`), and the shape
seeded whole (`Shape::fresh`): applied as they came, the rows of an alter
failed the put and the stream retried the same change for good. A one-row change
costs 18.4 us against 7.1 for the same put alone, rows in changes of 100
3.8 us either way, a 128-dim row under HNSW 482 against 465, a seed of 10
000 rows 72 ms against 44 (`make sync-bench`). It adds about 165 KB to the
native library -- 1.51 -> 1.68 MB on aarch64-apple-darwin, 1.68 -> 1.88 on
x86_64 Linux, about 77 KB of it the sync's own code -- and the browser module is built without it:
`fenec-abi`'s `sync` feature, which only `fenec-ffi` turns on. Compiled
in as dead code, it still moved LLVM's inlining and left the module 155
bytes larger; off, the module is the size it was, 512 400 bytes. Dart's is
`Fenec.openSynced`, since a static `sync` cannot sit beside the instance's
fsync `sync()`.

## Sync scenarios

**The browser's sync and the native core are held to one scenario file.**
Moving `FenecSync` onto `fenec_abi::sync` was measured at +21 KB brotli of
the browser module, so the two are written apart, and
`integrations/sync-scenarios.json` says what both do: 71 scenarios, each a
script of shapes, app writes and server events -- a seed, a change, a
stream dropped, the status each write's request is answered with, a seed
past the horizon, the network's signal, a token -- with what the replica,
the queue, the requests sent (method, path, body, the idempotency key
reused or new), the timers and the refusals are after every step.
`crates/fenec-abi/tests/scenarios.rs` drives the sans-IO core through it,
and `web/fenec.sync.scenarios.test.js` drives `FenecSync` through a fetch
and a clock the script plays, its replica persisted over an in-memory
IndexedDB so a restart is a page opened again; each writes the names that
passed to `target/sync-scenarios/`, and `make sync-scenarios-check` (in
`make test` where the module is built, and its own CI step) fails unless
both ran every one -- under `CI` the JS runner fails rather than skip
without `web/fenec.wasm`. A behaviour of either changes in the file first.
A step's values are the builders' texts, so a write's fields are written
in alphabetical order (the native runner's JSON sorts them, JavaScript
keeps them as given); `<name>` binds a value made at run time -- a key, an
`Idempotency-Key` -- and `@name` is a fixture. Where the platforms differ
on purpose a scenario says why in `differs` and a step holds
`expect_js`/`expect_native`; none does now. What is left between them is
the platforms': a JS write's promise settles with the server's answer
where a native write returns once kept, JS has `batch()` where a native
text of several statements is one write, tabs elect a leader, and the
browser fetches collation chunks. Bringing the browser to the core fixed
what it got wrong: a network failure or a 5xx put a write back, an update
or a delete of an unanswered insert went by the temporary id the server
never saw, an insert the shape did not hold kept its row until the next
seed, a seed cleared the collection and every pending row with it, a
write made offline was gone with the page, and each tab sent its own
writes. Its state is now the core's, in the replica: `_sync_shapes`,
`_sync_queue` and `_sync_temps`, stored the moment a write is made
(`persist`'s journal, or the file of an `openFile`d `local`); a follower
tab applies its write and hands it to the leader, which keeps it in its
own queue and tells every tab the answer, and a tab taking the lead reads
the queue its predecessor stored (`#lead`, through a `sibling` database).
`fenec-http`'s CORS allows `Idempotency-Key` and exposes `Fenec-Seq`,
without which a page on another origin could not send the one or read the
other. `fenec.js` grew 113.3 -> 132.5 KB, 31.1 -> 35.8 KB brotli -- 3.1 KB
of it code, the rest the comments -- and the browser module did not change
by a byte.
