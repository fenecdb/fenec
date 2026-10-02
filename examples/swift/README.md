# Notes -- Swift (SwiftUI, embedded)

A notes app for macOS whose database is a file in Application Support:
fenecdb embedded through the `FenecDB` package, no server. The same views
run on iOS.

## What it shows

- the schema in `Sources/NotesKit/schema.fenecql`, applied with
  `db.schema(...)` at every open, and four notes seeded into an empty file;
- creating notes, and marking one done with a tap;
- full-text search with `match`, fused with `near` over a toy embedding
  (`fuse`);
- filtering by tag (`tags has`) or by `done`, newest first (`order at desc`);
- a live query: the list is a `LiveQuery`, drawn again after every write;
- persistence: the file outlives the app, and opens where it stopped.

`NotesKit` holds the model (`Notes.swift`) and the views
(`NotesView.swift`); `NotesApp` is the macOS app, `notes-cli` the same
model from a terminal.

## Prerequisites

Swift 5.9 or newer on macOS 12+ (Xcode, or the Command Line Tools for the
app and the CLI). The package fetches `FenecDB` and its prebuilt library
from the release.

## Run

```sh
cd examples/swift
swift run NotesApp                       # the app; the file is ~/Library/Application Support/Notes/notes.fenec
swift run notes-cli                      # ./notes.fenec: seeds it, then lists
swift run notes-cli add "Dentist" "Call the dentist on Monday." health
swift run notes-cli list --tag home
swift run notes-cli list --open
swift run notes-cli search istanbul trip # match, then fuse
swift run notes-cli done 1
swift run notes-cli smoke                # the whole tour on a temporary file, checked
```

The CLI has no `watch`: a file is open in one process at a time (a second
open is refused), so nothing else could write what it would show. The live
query is the app's list, and `smoke` runs one.

## On iOS

`NotesKit` builds for iOS 15 as it is. In Xcode, make an iOS app, add this
package (File > Add Package Dependencies > Add Local...) and its `NotesKit`
library, and use the same two calls as `Sources/NotesApp/NotesApp.swift`:

```swift
let notes = try await Notes.open(Notes.defaultURL())   // Application Support/Notes/notes.fenec
NotesView(notes: notes)
```

## The core

```swift
let db = try await Fenec.open(url)
try await db.schema(schemaText)                          // schema.fenecql: made or checked
try await db.from("notes").insert(["title": "Dentist", "body": "Call on Monday.",
    "tags": ["health"], "done": false, "at": Date().fenecValue(), "embed": .floats(embed("..."))] as Row)

let open = try db.from("notes").where("done", false).order("at", "desc")
@StateObject var live = LiveQuery(db, open, as: Note.self)   // a SwiftUI list, again after every write

let hits = try await db.from("notes").match("body", words)
    .near("embed", embed(words)).fuse().limit(20).rows(as: Note.self)
try await db.from("notes").where("id", id).update(["done": true] as Row)
```

## The toy embedding

`embed(_:)` in `Notes.swift` is a placeholder, not a model: character
trigrams (of the UTF-8 bytes) hashed into 64 dimensions, so `near` and
`fuse` have something to rank without a download. Every Notes example
computes the same vectors. A real app computes them with a model -- Core ML
on the device, or an embeddings API -- and declares `vector<N>` with that
model's `N` in `schema.fenecql`.

## Against this repository's build

CI builds the example against the repository's `FenecDB` rather than the
release: with `FENEC_LOCAL` set, `Package.swift` takes the package at the
repository's root, and that takes the XCFramework
`integrations/swift/build-xcframework.sh` built.

```sh
integrations/swift/build-xcframework.sh --macos          # from the repository's root
cd examples/swift
FENEC_LOCAL=1 swift build
FENEC_LOCAL=1 swift run notes-cli smoke
```
