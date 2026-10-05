# FenecDB for Swift

fenecdb embedded in a macOS or iOS app: the database is a file on the
device, the engine the native library (`crates/fenec-ffi`) the server and
the browser module share their code and file format with -- on its own, or
a replica a server keeps in step.

```swift
.package(url: "https://github.com/fenecdb/fenec", from: "0.1.7")
// target: .product(name: "FenecDB", package: "fenec")
```

```swift
import FenecDB

let docs = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
let db = try await Fenec.open(docs.appendingPathComponent("app.fenec"))
try await db.execute("create collection todos (title text, done bool @hash)")

struct Todo: Codable, Sendable, FenecValue { var title: String; var done: Bool }
try await db.from("todos").insert(Todo(title: "milk", done: false))

let open = try await db.from("todos").where("done", false).rows(as: Todo.self)
let near = try await db.query("get notes near embed $1 limit 5", embedding)   // [Float]
```

In SwiftUI, `LiveQuery` (an `ObservableObject`) or `LiveRows` (`@Observable`,
iOS 17 and macOS 14) holds a query's rows and runs it again after a write to
a collection it reads, once a burst, on the main actor:

```swift
@StateObject var todos = LiveQuery(db, try! db.from("todos").where("done", false), as: Todo.self)
```

`db.live(query)` is the same as an `AsyncThrowingStream`. To see the text
and parameters a chain builds, for logging or a test, `try
query.toFenecQL()` returns them and runs nothing.

Every write is fsynced before it returns unless the file is opened
`.noSync`, which leaves the writes for `sync()`; `flush()` hands them to the
system, which outlives the app being killed. `checkpoint()` writes the file
anew, graphs and all. An open leaves the hash, text, ordered and sparse
indexes for their first read; `warm(["docs"])` builds them off the main
thread as the app starts, so the first search does not (a `@text` index of
100 000 products: 141 ms), and answers how many it built. The builder's
`facet(field, ranges: [0, 25, 50])` counts by ranges of numbers, each value
`[from, to]`, and `disjunctive: true` counts past the filter's own
condition on the field. The docs: `site/content/docs/mobile.html`.

## Sync with a server

```swift
let db = try await Fenec.sync(url: "https://api.example.com", token: jwt,
                              shapes: [Shape("todos", where: ["owner": me], key: "key")],
                              path: docs.appendingPathComponent("todos.fenec").path,
                              tokenProvider: { try await refreshToken() })
await db.replica!.ready()
```

The same `Fenec`: reads and live queries are the file's, a write to a
shape's collection shows at once and is sent -- queued in the file while
the server cannot be reached, under an idempotency key so it lands once --
and one the server refuses is put back and comes on
`db.replica!.refusals()`. `replica.status` says `online`, `offline` or
`catchingUp`, the writes not yet answered and the last error; call
`replica.resume()` as the app comes back to the foreground. The requests and
the stream are `URLSession`'s, so a server is reached through TLS (App
Transport Security). `Fenec.connect(url:token:)` is a server with no file:
every query a request, the same builder. Its `batch([...], idempotencyKey:)`
runs the builder's `toInsert`/`toUpdate`/`toDelete` as one block, all or
none, answering `results`, `seq` and `replayed`; a `FenecError` that stops
it carries `status` (412 and `.unmet` for a `require` not met) and `at`,
the statement from 0. `run(text, params:, idempotencyKey:)` and
`withIdempotencyKey(key)`, a copy whose every write carries the key, make a
write once however often it is sent.

## Building and testing

`make swift-test` builds the XCFramework's macOS slice for this machine
(`integrations/swift/build-xcframework.sh --macos`) and runs `swift test`
at the repository's root, where `Package.swift` is (the sync tests start a
`fenec-server` it builds, macOS alone) -- SwiftPM fetches a
package by its repository, so the manifest lives there. It takes the
XCFramework from `integrations/swift/build/` when it is there and from the
release's download otherwise. `build-xcframework.sh` alone builds every
slice: macOS (arm64, x86_64), iOS, and the simulator (arm64, x86_64).
It then runs the tests again with Swift's cooperative pool cut to one
thread (`test-strict.sh`, `LIBDISPATCH_COOPERATIVE_POOL_STRICT=1`), where
a call that blocks a thread of the pool hangs at once rather than now and
then on a device of few cores; a run that hangs is sampled and fails.
