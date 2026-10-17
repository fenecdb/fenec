// Reads near a tenant's read replica (`Fenec.connect(url:reads: .nearest)`):
// against real routers and nodes (Routers.swift), and what a connection
// sends after its answers against a script -- a failover's 504, answers
// arriving out of order, a tenant made again. macOS alone, as the sync
// tests: the iOS simulator's tests cannot start a process.
#if os(macOS)
    import Testing

    @testable import FenecDB

    private let h1 = "1-00000000000000aa"
    private let h2 = "2-00000000000000bb"

    /// `<term>-<id>`, the id 16 hex digits: what a node that replicates
    /// names a tenant's history.
    private func isHistory(_ s: String) -> Bool {
        let parts = s.split(separator: "-")
        return parts.count == 2 && !parts[0].isEmpty && parts[0].allSatisfy(\.isNumber) && parts[1].count == 16
            && parts[1].allSatisfy(\.isHexDigit)
    }

    private func written(_ seq: String, _ history: String) -> Scripted {
        Scripted(body: #"{"affected":1}"#, seq: seq, history: history)
    }

    private func refused(_ seq: Int, _ history: String, gate: Once<Bool>? = nil) -> Scripted {
        Scripted(status: 504, body: #"{"error":"not in this copy","seq":\#(seq),"history":"\#(history)"}"#, gate: gate)
    }

    @Suite(.serialized) struct Reads {
        /// The builder's reads, and an `asRead()` copy's, are answered by the
        /// replica near the app -- shown by an answer while home cannot be
        /// reached, which only the replica could give -- and each is sent
        /// after the connection's last write, whichever copy wrote it, so
        /// it never reads older than what it wrote; writes, and every
        /// request of a connection reading home, go home.
        @Test func readsNearAReadReplicaAreAnsweredThereNeverOlderThanTheAppWrote() async throws {
            let w = try await ReadReplicas.start("near")
            defer { w.stop() }
            let sent = Wire.Log(script: nil)
            let db = connection(w.url, reads: .nearest, log: sent)
            let home = Fenec.connect(url: w.url)

            // A write goes home, unmarked -- marked, it would be refused --
            // and says where it left the tenant.
            #expect(try await db.from("notes").insert(["body": "a"] as Value) == 1)
            let a = try #require(db.seq)
            #expect(sent.last["fenec-read"] == nil && sent.last["fenec-after"] == nil)
            // The builder's read after it waits for it on the replica: that
            // change of the history the write named, which a node that
            // replicates gives each tenant.
            #expect(try await db.from("notes").rows().compactMap { $0["body"]?.string } == ["a"])
            #expect(sent.last["fenec-read"] == "replica")
            #expect(sent.last["fenec-after"] == String(a))
            let history = try #require(sent.last["fenec-history"])
            #expect(isHistory(history))

            // The copies share the position: a keyed copy's write is what
            // the connection's next read is sent after, and an `asRead()`
            // copy's reads go after the connection's writes.
            #expect(try await db.withIdempotencyKey("note-b").from("notes").insert(["body": "b"] as Value) == 1)
            let b = try #require(db.seq)
            #expect(b > a)
            #expect(sent.last["fenec-read"] == nil && sent.last["idempotency-key"] == "note-b")
            #expect(try await db.from("notes").count() == 2)
            #expect(sent.last["fenec-read"] == "replica" && sent.last["fenec-after"] == String(b))
            let reader = db.asRead()
            let both = try await reader.batch([
                try db.from("notes").where("body", "a").toFenecQL(),
                try db.from("notes").where("body", "b").toFenecQL(),
            ])
            #expect(both.results.map(\.rows.count) == [1, 1])
            #expect(sent.last["fenec-read"] == "replica" && sent.last["fenec-after"] == String(b))
            #expect(try await reader.query("get notes count").first?["count"]?.int == 2)
            #expect(sent.last["fenec-read"] == "replica" && sent.last["fenec-history"] == history)
            // A text not marked goes home.
            _ = try await db.query("get notes count")
            #expect(sent.last["fenec-read"] == nil && sent.last["fenec-after"] == nil)
            // A write marked by mistake is refused, never landed.
            do {
                try await reader.execute(#"put notes {body: "x"}"#)
                Issue.record("a marked write was answered")
            } catch let e as FenecError {
                #expect(e.status == 403)
            }

            // Home gone: the reads near are still answered -- by the
            // replica, the one copy left -- and the connection reading home
            // is not. The replica asked itself first, until it holds both: a
            // read near above may have been answered home, the replica not
            // caught up yet.
            try await eventually("the read replica holds both writes") { try await w.replicaCount() == 2 }
            await w.eu1.kill()
            #expect(try await db.from("notes").order("body").rows().compactMap { $0["body"]?.string } == ["a", "b"])
            #expect(sent.last["fenec-read"] == "replica" && sent.last["fenec-after"] == String(b))
            do {
                _ = try await home.from("notes").rows()
                Issue.record("home answered with no node")
            } catch let e as FenecError {
                #expect(e.status == 502)
            }
            do {
                try await db.from("notes").insert(["body": "c"] as Value)
                Issue.record("a write was answered with no home")
            } catch let e as FenecError {
                #expect(e.status == 502)
            }
        }

        /// A connection reading home marks nothing, `asRead()` or not.
        @Test func aConnectionReadsHomeUnlessItAsksForNearest() async throws {
            let sent = Wire.Log(script: [])
            let db = connection("http://home.invalid", reads: .home, log: sent)
            _ = try await db.from("t").rows()
            _ = try await db.asRead().query("get t count")
            #expect(sent.all.count == 2)
            #expect(sent.all.allSatisfy { $0["fenec-read"] == nil && $0["fenec-after"] == nil })
            #expect(Fenec.connect(url: "http://home.invalid").reads == .home)
        }

        /// Reads near go after the furthest position an answer from home
        /// named, through any copy -- not the last answered, and not a read
        /// near's own answer, which may be a replica's. A read no copy holds
        /// that for (504) is asked home with neither, and home's position,
        /// which the 504 names, is the connection's from then.
        @Test func readsNearAreSentAfterTheFurthestPositionFromHome() async throws {
            let sent = Wire.Log(script: [
                written("9", h1), written("12", h1), written("10", h1),
                Scripted(body: #"[{"n":1}]"#), Scripted(body: #"[{"count":2}]"#), Scripted(body: #"[{"count":2}]"#),
                Scripted(body: #"{"results":[]}"#, seq: "15", history: h1),
                Scripted(status: 504, body: #"{"error":"change 12 is not in this copy's history","seq":10,"history":"\#(h2)"}"#),
                Scripted(body: #"[{"n":1}]"#), Scripted(body: #"[{"n":1}]"#),
            ])
            let db = connection("http://script.invalid", reads: .nearest, log: sent)
            try await db.withIdempotencyKey("k1").execute("put t {n: 1}")
            try await db.withWait(1).execute("set t {n: 2} where n = 1")
            try await db.execute("put t {n: 3}")
            _ = try await db.from("t").rows()
            #expect(try await db.from("t").count() == 2)
            _ = try await db.query("get t count")
            _ = try await db.asRead().batch([try db.from("t").toFenecQL()])
            #expect(try await db.withWait(1).from("t").rows().count == 1)
            _ = try await db.from("t").rows()

            let seen = sent.all
            #expect(seen.count == 10)
            for h in seen[0..<3] { #expect(h["fenec-read"] == nil && h["fenec-after"] == nil) }
            // The furthest position, 12, not the last answered, 10.
            for i in [3, 4, 6, 7] {
                #expect(seen[i]["fenec-read"] == "replica" && seen[i]["fenec-after"] == "12" && seen[i]["fenec-history"] == h1)
            }
            for i in [5, 8] {
                #expect(seen[i]["fenec-read"] == nil && seen[i]["fenec-after"] == nil && seen[i]["fenec-history"] == nil)
            }
            // Home's position taken, lower and of the history after the
            // failover.
            #expect(seen[9]["fenec-after"] == "10" && seen[9]["fenec-history"] == h2)
            #expect(db.seq == 10)
        }

        /// A read near's 504 sets the position back only over the one the
        /// read was sent after -- a write answered meanwhile is further, and
        /// set back the next read near would miss it -- and never to an
        /// earlier term, which a fenced old primary names.
        @Test func a504SetsThePositionBackOverItsOwnAloneNeverToAnEarlierTerm() async throws {
            let sent = Wire.Log(script: [written("100", h2)])
            let db = connection("http://race.invalid", reads: .nearest, log: sent)
            try await db.execute("put t {n: 1}")

            // A read near is sent after 100; a write lands at 101 while it
            // waits; then home answers the read 504 at 100.
            let held = Once<Bool>()
            sent.add(refused(100, h2, gate: held))
            let read = Task { try await db.from("t").rows() }
            try await eventually("the read sent") { sent.all.count == 2 }
            #expect(sent.last["fenec-after"] == "100")
            sent.add(written("101", h2))
            try await db.execute("put t {n: 2}")
            held.set(true)
            _ = try await read.value
            _ = try await db.from("t").rows()
            #expect(sent.last["fenec-after"] == "101")

            // A 504 naming an earlier term -- the old primary, fenced,
            // reached through a router behind a failover -- is not taken.
            sent.add(refused(140, h1))
            _ = try await db.from("t").rows()
            _ = try await db.from("t").rows()
            #expect(sent.last["fenec-after"] == "101" && sent.last["fenec-history"] == h2)
        }

        /// A tenant made again under its name starts again at term 1, of
        /// another lineage -- the top half of its history's id: home's
        /// position is taken, from a 504 and from an answer, its term lower.
        /// Refused as an earlier term, every read near waited its whole wait
        /// for good. Within one lineage an earlier term is still refused.
        @Test func aTenantMadeAgainHasItsPositionTakenItsTermLower() async throws {
            let old = "3-aaaaaaaa000000c3", remade = "1-bbbbbbbb00000001"
            let third = "1-cccccccc00000001", second = "2-cccccccc00000002"
            let sent = Wire.Log(script: [
                written("50", old), refused(2, remade), Scripted(), Scripted(),
                written("1", third), Scripted(), written("9", second), written("90", third), Scripted(),
            ])
            let db = connection("http://lineage.invalid", reads: .nearest, log: sent)
            try await db.execute("put t {n: 1}")
            _ = try await db.from("t").rows()
            _ = try await db.from("t").rows()
            #expect(sent.last["fenec-after"] == "2" && sent.last["fenec-history"] == remade)
            try await db.execute("put t {n: 2}")
            _ = try await db.from("t").rows()
            #expect(sent.last["fenec-after"] == "1" && sent.last["fenec-history"] == third)
            try await db.execute("put t {n: 3}")
            try await db.execute("put t {n: 4}")
            _ = try await db.from("t").rows()
            #expect(sent.last["fenec-after"] == "9" && sent.last["fenec-history"] == second)
        }
    }
#endif
