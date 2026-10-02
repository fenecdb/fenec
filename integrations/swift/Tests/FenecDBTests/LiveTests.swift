import Testing

@testable import FenecDB

/// Waits on the main actor until `done` holds, or fails after a few seconds.
@MainActor
func until(_ what: String, _ done: () -> Bool) async {
    for _ in 0..<300 {
        if done() { return }
        try? await Task.sleep(nanoseconds: 10_000_000)
    }
    Issue.record("timed out waiting for \(what)")
}

@Suite(.serialized) @MainActor struct Live {
    func todos() async throws -> Fenec {
        let db = try Fenec.memory()
        try await db.execute(
            "create collection todos (title text, done bool @hash); create collection other (n int)")
        try await db.from("todos").insert([["title": "milk", "done": false], ["title": "bread", "done": true]] as Value)
        return db
    }

    @Test func aStreamHasItsRowsNowAndAfterEachWrite() async throws {
        let db = try await todos()
        let open = try db.from("todos").select("title").where("done", false)
        var seen: [[String]] = []
        let stream = db.live(open)
        let reader = Task { @MainActor in
            for try await rows in stream {
                seen.append(rows.compactMap { $0["title"]?.string })
                if seen.count == 3 { break }
            }
        }
        await until("the first rows") { seen.count == 1 }
        #expect(seen == [["milk"]])
        try await db.from("todos").insert(["title": "eggs", "done": false] as Value)
        await until("the rows after a put") { seen.count == 2 }
        #expect(seen.last == ["milk", "eggs"])
        // A write to a collection it does not read runs nothing.
        try await db.execute("put other {n: 1}")
        try await Task.sleep(nanoseconds: 80_000_000)
        #expect(seen.count == 2)
        try await db.from("todos").where("title", "milk").update(["done": true] as Value)
        await until("the rows after a set") { seen.count == 3 }
        #expect(seen.last == ["eggs"])
        _ = try await reader.value
        try await db.close()
    }

    /// The writes of a burst -- several at once, and a text of several
    /// statements -- run the query once.
    @Test func aBurstOfWritesRunsItOnce() async throws {
        let db = try await todos()
        let all = try db.from("todos")
        let live = LiveQuery(db, all)
        await until("the first rows") { live.loaded }
        var runs = 0
        let watch = live.$rows.dropFirst().sink { _ in runs += 1 }
        try await withThrowingTaskGroup(of: Void.self) { group in
            for i in 0..<10 {
                group.addTask { try await all.insert(["title": "t\(i)", "done": false] as Value) }
            }
            try await group.waitForAll()
        }
        await until("the rows after the burst") { live.rows.count == 12 }
        try await Task.sleep(nanoseconds: 100_000_000)
        #expect(runs == 1)
        try await db.execute("put todos {title: \"x\"}; put todos {title: \"y\"}; put other {n: 2}")
        await until("the rows after a text") { live.rows.count == 14 }
        try await Task.sleep(nanoseconds: 100_000_000)
        #expect(runs == 2)
        // A text that fails is put back whole and runs nothing.
        await #expect(throws: FenecError.self) { try await db.execute("put todos {title: \"z\"}; put other {n: \"no\"}") }
        try await Task.sleep(nanoseconds: 100_000_000)
        #expect(runs == 2)
        live.stop()
        try await all.insert(["title": "after", "done": false] as Value)
        try await Task.sleep(nanoseconds: 100_000_000)
        #expect(runs == 2)
        watch.cancel()
        try await db.close()
    }

    struct Todo: Codable, Sendable, Equatable {
        var title: String
        var done: Bool
    }

    @Test func anObservableDecodesItsRows() async throws {
        let db = try await todos()
        let live = LiveQuery(db, try db.from("todos").order("title"), as: Todo.self)
        await until("the first rows") { live.loaded }
        #expect(live.rows == [Todo(title: "bread", done: true), Todo(title: "milk", done: false)])
        try await db.from("todos").where("title", "bread").delete()
        await until("the rows after a delete") { live.rows.count == 1 }
        #expect(live.error == nil)
        if #available(macOS 14, iOS 17, *) {
            let rows = LiveRows(db, try db.from("todos").select("title"))
            await until("the observable's rows") { rows.loaded }
            #expect(rows.rows.map { $0["title"]?.string } == ["milk"])
            rows.stop()
        }
        try await db.close()
    }

    /// A text names the collections it reads, or runs after every write; a
    /// drop runs everything, the dropped collection's query to its error.
    @Test func aTextAndADrop() async throws {
        let db = try await todos()
        let text = LiveQuery(db, "get todos count", collections: ["todos"])
        let any = LiveQuery(db, "get other count")
        await until("the first rows") { text.loaded && any.loaded }
        try await db.execute("put todos {title: \"t\"}")
        await until("the count") { text.rows.first?["count"] == .int(3) }
        try await db.execute("drop collection other")
        await until("the error") { any.error != nil }
        #expect(any.error?.code == .notFound)
        let errors = db.live(try db.from("nowhere"))
        await #expect(throws: FenecError.self) { for try await _ in errors {} }
        try await db.close()
    }
}
