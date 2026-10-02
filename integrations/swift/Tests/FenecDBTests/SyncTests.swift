// A replica against a real fenec-server the tests start (Servers.swift):
// macOS alone, since the iOS simulator's tests cannot start a process.
#if os(macOS)
    import Testing

    @testable import FenecDB

    /// Waits until `done` holds, or fails after ten seconds.
    func eventually(_ what: String, _ done: () async throws -> Bool) async throws {
        for _ in 0..<1000 {
            if try await done() { return }
            try await Task.sleep(nanoseconds: 10_000_000)
        }
        Issue.record("timed out waiting for \(what)")
    }

    let open = Shape("tasks", where: ["status": "open"], key: "key")

    func titles(_ db: Fenec) async throws -> [String] {
        try await db.from("tasks").select("title").order("title").rows().compactMap { $0["title"]?.string }
    }

    @Suite(.serialized) struct Sync {
        @Test func theSeedFillsTheReplicaWithTheShape() async throws {
            let s = try await Server("seed")
            defer { s.stop() }
            let db = try await Fenec.sync(url: s.url, shapes: [open], path: try scratch("sync-seed"))
            await db.replica!.ready()
            #expect(try await titles(db) == ["one", "two"])
            #expect(db.replica!.status.state == .online)
            // Reads are the file's: the server gone, they go on.
            s.kill()
            #expect(try await db.from("tasks").count() == 2)
            try await db.close()
        }

        @Test func aLiveQueryRunsAgainOnAServerWrite() async throws {
            let s = try await Server("live")
            defer { s.stop() }
            let db = try await Fenec.sync(url: s.url, shapes: [open], path: try scratch("sync-live"))
            await db.replica!.ready()
            let seen = Box<[[String]]>([])
            let reader = Task {
                for try await rows in db.live(try db.from("tasks").select("title").order("title")) {
                    seen.set { $0 + [rows.compactMap { $0["title"]?.string }] }
                }
            }
            try await eventually("the first rows") { seen.get().count == 1 }
            try await s.run(#"put tasks {key: "d", title: "four", status: "open"}"#)
            try await eventually("the server's write") { seen.get().last == ["four", "one", "two"] }
            reader.cancel()
            try await db.close()
        }

        @Test func anOptimisticWriteShowsAtOnceAndLands() async throws {
            let s = try await Server("optimistic")
            defer { s.stop() }
            let db = try await Fenec.sync(url: s.url, shapes: [open], path: try scratch("sync-optimistic"))
            await db.replica!.ready()
            try await db.from("tasks").insert(["title": "new", "status": "open", "priority": 2] as Value)
            // At once, under a temporary id.
            let row = try await db.from("tasks").where("title", "new").first()
            #expect((row?["id"]?.int ?? 0) >= 1 << 52)
            await db.replica!.pushed()
            try await eventually("the server's copy in place of the temporary row") {
                let rows = try await db.from("tasks").where("title", "new").rows()
                return rows.count == 1 && (rows[0]["id"]?.int ?? 0) < 1 << 52
            }
            #expect(try await s.run(#"get tasks where title = "new""#).rows.count == 1)
            // An update and a delete go the same way.
            try await db.from("tasks").where("key", "a").update(["title": "ONE"] as Value)
            try await db.from("tasks").where("key", "b").delete()
            await db.replica!.pushed()
            #expect(try await s.run(#"get tasks where title = "ONE""#).rows.count == 1)
            #expect(try await s.run(#"get tasks where key = "b""#).rows.isEmpty)
            try await db.close()
        }

        @Test func aRefusedWriteIsPutBack() async throws {
            let s = try await Server("refused")
            defer { s.stop() }
            let db = try await Fenec.sync(url: s.url, shapes: [open], path: try scratch("sync-refused"))
            await db.replica!.ready()
            let refusals = db.replica!.refusals()
            let refused = Box<[Refusal]>([])
            let reader = Task {
                for await r in refusals { refused.set { $0 + [r] } }
            }
            // The server's key is @unique; the replica's a plain hash.
            try await db.from("tasks").insert(["key": "a", "title": "dup", "status": "open"] as Value)
            #expect(try await titles(db) == ["dup", "one", "two"])
            try await eventually("the refusal") { !refused.get().isEmpty }
            #expect(refused.get().first?.status == 409)
            reader.cancel()
            try await eventually("the write put back") { try await titles(db) == ["one", "two"] }
            #expect(db.replica!.status.error?.status == 409)
            #expect(await db.replica!.refresh().pending == 0)
            try await db.close()
        }

        @Test func aServerKilledAndStartedAgainIsCaughtUpWith() async throws {
            let s = try await Server("restart")
            defer { s.stop() }
            let db = try await Fenec.sync(url: s.url, shapes: [open], path: try scratch("sync-restart"))
            await db.replica!.ready()
            s.kill()
            try await eventually("offline") { db.replica!.status.state == .offline }
            // A write while it is down waits in the file.
            try await db.from("tasks").insert(["title": "while down", "status": "open"] as Value)
            #expect(await db.replica!.refresh().pending == 1)
            try s.restart()
            try await s.run(#"put tasks {key: "e", title: "after", status: "open"}"#)
            db.replica!.resume()
            try await eventually("caught up both ways") {
                try await titles(db) == ["after", "one", "two", "while down"] && db.replica!.status.pending == 0
            }
            #expect(try await s.run(#"get tasks where title = "while down""#).rows.count == 1)
            try await db.close()
        }

        @Test func pendingWritesOutliveAReopen() async throws {
            let s = try await Server("reopen")
            defer { s.stop() }
            let path = try scratch("sync-reopen")
            var db = try await Fenec.sync(url: s.url, shapes: [open], path: path)
            await db.replica!.ready()
            s.kill()
            try await db.from("tasks").insert(["title": "kept", "status": "open"] as Value)
            try await db.from("tasks").where("key", "a").update(["title": "ONE"] as Value)
            #expect(await db.replica!.refresh().pending == 2)
            try await db.close()

            db = try await Fenec.sync(url: s.url, shapes: [open], path: path)
            #expect(await db.replica!.refresh().pending == 2)
            #expect(try await titles(db) == ["ONE", "kept", "two"])
            try s.restart()
            db.replica!.resume()
            try await eventually("the queue sent") { db.replica!.status.pending == 0 }
            #expect(try await s.run(#"get tasks where title = "kept""#).rows.count == 1)
            #expect(try await s.run(#"get tasks where title = "ONE""#).rows.count == 1)
            try await eventually("the server's copies") {
                try await db.from("tasks").rows().allSatisfy { ($0["id"]?.int ?? 0) < 1 << 52 }
            }
            try await db.close()
        }

        /// The Swift tab's "over HTTP" lines on languages.html, as written
        /// there but for the server's address.
        @Test func theDocsExampleOverHTTP() async throws {
            let s = try await Server("docs")
            defer { s.stop() }
            try await s.run("create collection if not exists docs (title text, embed vector<3> @hnsw(cosine))")
            try await s.run(#"put docs {title: "Night at the oasis", embed: [0.1, 0.2, 0.3]}"#)

            let remote = Fenec.connect(url: s.url, token: "secret")
            try await remote.execute("put docs {title: $1, embed: $2}", "Dunes", [Float(0.9), 0.1, 0.0])
            let hits = try await remote.from("docs").select("title").near("embed", [Float(0.1), 0.2, 0.3]).limit(5).rows()
            #expect(hits.compactMap { $0["title"]?.string } == ["Night at the oasis", "Dunes"])
        }

        @Test func connectRunsEveryQueryOnTheServer() async throws {
            let s = try await Server("connect")
            defer { s.stop() }
            let db = Fenec.connect(url: s.url)
            let rows = try await db.from("tasks").where("status", "open").order("priority", "desc").rows()
            #expect(rows.compactMap { $0["title"]?.string } == ["two", "one"])
            #expect(try await db.from("tasks").insert(["key": "z", "title": "remote"] as Value) == 1)
            #expect(try await db.from("tasks").count() == 4)
            await #expect(throws: FenecError.self) {
                try await db.from("tasks").insert(["key": "z", "title": "again"] as Value)
            }
        }
    }

    /// A value the tests' tasks share.
    final class Box<T: Sendable>: @unchecked Sendable {
        private var value: T
        private let lock = NSLockish()
        init(_ v: T) { value = v }
        func get() -> T { lock.with { value } }
        func set(_ f: (T) -> T) { lock.with { value = f(value) } }
    }
#endif
