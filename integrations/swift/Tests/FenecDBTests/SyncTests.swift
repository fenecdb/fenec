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
            await s.kill()
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
            // Offline, nothing is sent: the server's answer cannot replace
            // the row before it is read (see aRefusedWriteIsPutBack).
            db.replica!.setOnline(false)
            try await db.from("tasks").insert(["title": "new", "status": "open", "priority": 2] as Value)
            // At once, under a temporary id.
            let row = try await db.from("tasks").where("title", "new").first()
            #expect((row?["id"]?.int ?? 0) >= 1 << 52)
            db.replica!.setOnline(true)
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
            // Offline, nothing is sent, so the row is read before the server
            // can refuse it: online, the 409 could come back and the row be
            // put back before the read, as it was in the Kotlin test on a
            // loaded runner.
            db.replica!.setOnline(false)
            // The server's key is @unique; the replica's a plain hash.
            try await db.from("tasks").insert(["key": "a", "title": "dup", "status": "open"] as Value)
            #expect(try await titles(db) == ["dup", "one", "two"])
            db.replica!.setOnline(true)
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
            await s.kill()
            try await eventually("offline") { db.replica!.status.state == .offline }
            // A write while it is down waits in the file.
            try await db.from("tasks").insert(["title": "while down", "status": "open"] as Value)
            #expect(await db.replica!.refresh().pending == 1)
            try await s.restart()
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
            await s.kill()
            try await db.from("tasks").insert(["title": "kept", "status": "open"] as Value)
            try await db.from("tasks").where("key", "a").update(["title": "ONE"] as Value)
            #expect(await db.replica!.refresh().pending == 2)
            try await db.close()

            db = try await Fenec.sync(url: s.url, shapes: [open], path: path)
            #expect(await db.replica!.refresh().pending == 2)
            #expect(try await titles(db) == ["ONE", "kept", "two"])
            try await s.restart()
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

        /// A batch lands whole or not at all, says which statement stopped
        /// it, and under an idempotency key lands once.
        @Test func connectBatchesAndKeysWrites() async throws {
            let s = try await Server("batch")
            defer { s.stop() }
            let db = Fenec.connect(url: s.url)
            let tasks = try db.from("tasks")
            let out = try await db.batch([
                try tasks.toInsert(["key": "d", "title": "four"] as Value),
                try tasks.where("key", "a").toUpdate(["priority": 9] as Value, require: 1),
                try tasks.where("priority", ">=", 5).order("priority").select("key").toFenecQL(),
            ])
            #expect(out.results.count == 3)
            #expect(out.results[0].affected == 1)
            #expect(out.results[2].rows.compactMap { $0["key"]?.string } == ["b", "a"])
            #expect(out.seq != nil && out.seq == db.seq && !out.replayed)

            do {
                _ = try await db.batch([
                    try tasks.toInsert(["key": "e", "title": "five"] as Value),
                    try tasks.where("key", "nobody").toDelete(require: 1),
                ])
                Issue.record("an unmet batch was answered")
            } catch let e as FenecError {
                #expect(e.code == .unmet && e.status == 412 && e.at == 1 && e.completed == 0)
            }
            #expect(try await tasks.count() == 4)

            let keyed = db.withIdempotencyKey("batch-1")
            let stmts = [try tasks.toInsert(["key": "f", "title": "six"] as Value)]
            #expect(try await keyed.batch(stmts).replayed == false)
            #expect(try await keyed.batch(stmts).replayed == true)
            #expect(try await db.from("tasks").insert(["key": "g"] as Value) == 1)
            #expect(try await keyed.from("tasks").count() == 6)
            _ = try await db.run(#"put tasks {key: "h"}"#, params: [], idempotencyKey: "put-1")
            _ = try await db.run(#"put tasks {key: "h"}"#, params: [], idempotencyKey: "put-1")
            #expect(try await tasks.count() == 7)
            do {
                _ = try await db.run(#"put tasks {key: "i"}"#, params: [], idempotencyKey: "put-1")
                Issue.record("a key sent with another request was answered")
            } catch let e as FenecError {
                #expect(e.status == 422 && e.at == nil)
            }
        }

        /// A claim held at the server until a job comes (`withWait`): taken
        /// once one is enqueued, ended on time with no row, and given up by
        /// cancelling its task.
        @Test func connectHoldsAClaimUntilAJobComes() async throws {
            let s = try await Server("held")
            defer { s.stop() }
            try await s.run("create collection jobs (kind text, run_at timestamp @sorted, owner text)")
            let db = Fenec.connect(url: s.url)
            @Sendable func claim(_ c: FenecRemote, _ owner: String) async throws -> [Row] {
                try await c.from("jobs").where(.raw("run_at <= now()")).order("run_at").limit(1)
                    .updateReturning(["owner": .string(owner), "run_at": .expr("now() + ?", 60000)] as Value, returning: ["kind"])
            }
            let held = Task { try await claim(db.withWait(10), "w1") }
            try await Task.sleep(nanoseconds: 200_000_000)
            try await db.execute(#"put jobs {kind: "mail", run_at: now()}"#)
            #expect(try await held.value.compactMap { $0["kind"]?.string } == ["mail"])
            #expect(try await claim(db.withWait(0.3), "w2").isEmpty)
            let given = Task { try await claim(db.withWait(10), "w3") }
            try await Task.sleep(nanoseconds: 100_000_000)
            given.cancel()
            do {
                _ = try await given.value
                Issue.record("a cancelled claim was answered")
            } catch {}
        }

        /// `/query` answers `{"rows", "facets"}` when facets were asked, and
        /// the bare array otherwise; a mark is a column of the rows either way.
        @Test func connectReadsFacetsAndMarks() async throws {
            let s = try await Server("facets")
            defer { s.stop() }
            try await s.run("create collection docs (kind text, body text @text)")
            try await s.run(#"put docs [{kind: "a", body: "rust is fast"}, {kind: "a", body: "rust"}, {kind: "b", body: "go"}]"#)
            let db = Fenec.connect(url: s.url)
            let got = try await db.from("docs").highlight("body").match("body", "rust").facet("kind").answer()
            #expect(got.rows.count == 2)
            #expect(got.rows.allSatisfy { $0["highlight(body)"] == [[0, 4]] })
            #expect(got.facets["kind"] == [FacetCount(value: "a", count: 2)])
            let all = try await db.from("docs").facet("kind").limit(1).answer()
            #expect(all.rows.count == 1)
            #expect(all.facets["kind"] == [FacetCount(value: "a", count: 2), FacetCount(value: "b", count: 1)])
            #expect(try await db.from("docs").answer().facets.isEmpty)
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
