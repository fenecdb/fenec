# FenecDB for Swift

fenecdb embedded in a macOS or iOS app: the database is a file on the
device, the engine the native library (`crates/fenec-ffi`) the server and
the browser module share their code and file format with. No server, no
network.

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
anew, graphs and all. The docs: `site/content/docs/mobile.html`.

## Building and testing

`make swift-test` builds the XCFramework's macOS slice for this machine
(`integrations/swift/build-xcframework.sh --macos`) and runs `swift test`
at the repository's root, where `Package.swift` is -- SwiftPM fetches a
package by its repository, so the manifest lives there. It takes the
XCFramework from `integrations/swift/build/` when it is there and from the
release's download otherwise. `build-xcframework.sh` alone builds every
slice: macOS (arm64, x86_64), iOS, and the simulator (arm64, x86_64).
