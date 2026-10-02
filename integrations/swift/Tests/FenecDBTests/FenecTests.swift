import Testing

@testable import FenecDB

struct Note: Codable, Equatable, FenecValue {
    var title: String
    var stars: Int
    var embed: [Float]
}

@Suite struct Engine {
    @Test func aStatementAndItsRows() async throws {
        let db = try Fenec.memory()
        let created = try await db.run("create collection notes (title text, stars int, embed vector<3> @hnsw(cosine))")
        #expect(created == .ok("collection `notes` created"))
        let n = try await db.execute(
            "put notes [{title: $1, stars: 5, embed: $2}, {title: $3, stars: 3, embed: $4}]",
            "oasis", [Float(1), 0, 0], "dunes", [Float(0), 1, 0])
        #expect(n == 2)
        let rows = try await db.query("get notes select title, stars order stars desc")
        #expect(rows.map { $0["title"]?.string } == ["oasis", "dunes"])
        #expect(rows[0]["stars"] == .int(5))
        #expect(rows[0].keys == ["title", "stars"])
        try await db.close()
    }

    /// `near` scores as the engine computes them, to the `Float`: the
    /// vector goes over as its bytes and comes back as the shortest text of
    /// each `Float`.
    @Test func nearScoresAreExact() async throws {
        let db = try Fenec.memory()
        try await db.execute("create collection v (n int, e vector<2> @hnsw(l2))")
        try await db.execute("put v [{n: 1, e: [1, 0]}, {n: 2, e: [0, 1]}, {n: 3, e: [0.5, 0.25]}]")
        let q: [Float] = [1, 0]
        let rows = try await db.query("get v select n near e $1 limit 3", q)
        #expect(rows.map { $0["n"]?.int } == [1, 3, 2])
        let scores = rows.map { Float($0["_score"]!.double!) }
        let third: Float = ((0.5 - 1) * (0.5 - 1) + 0.25 * 0.25 as Float).squareRoot()
        #expect(scores == [0, third, Float(2).squareRoot()])
        // A list of `Double`s is a vector too, as a page's numbers are.
        let again = try await db.query("get v select n near e $1 limit 1", [0.0, 1.0] as [Double])
        #expect(again.first?["n"] == .int(2))
        let stored = try await db.query("get v select e where n = 3")
        #expect(stored.first?["e"]?.floats == [0.5, 0.25])
        try await db.close()
    }

    @Test func rowsDecodeAndDocumentsEncodeInTheirOrder() async throws {
        let db = try Fenec.memory()
        try await db.execute("create collection notes (title text, stars int, embed vector<2>)")
        let notes = try db.from("notes")
        #expect(try notes.toInsert(Note(title: "a", stars: 4, embed: [0.5, 0.25])).text
            == "put notes {title: $1, stars: $2, embed: $3}")
        try await notes.insert([Note(title: "a", stars: 4, embed: [0.5, 0.25]), Note(title: "b", stars: 2, embed: [1, 0])])
        let got = try await notes.order("stars", "desc").rows(as: Note.self)
        #expect(got == [Note(title: "a", stars: 4, embed: [0.5, 0.25]), Note(title: "b", stars: 2, embed: [1, 0])])
        let typed = try await db.query(as: Note.self, "get notes where stars < $1", 3)
        #expect(typed.map(\.title) == ["b"])
        #expect(try await notes.where("title", "a").first(as: Note.self)?.stars == 4)
        #expect(try await notes.count() == 2)
        #expect(try await notes.where("stars", ">", 3).update(["stars": 5] as Value) == 1)
        #expect(try await notes.where("stars", 5).delete() == 1)
        try await db.close()
    }

    /// A json field keeps a list of numbers as written: sent as `Float`s,
    /// the library asks for it again as JSON, which the binding does.
    @Test func aJsonFieldTakesItsListAsWritten() async throws {
        let db = try Fenec.memory()
        try await db.execute("create collection t (meta json)")
        try await db.execute("put t {meta: $1}", [0.1, 0.2] as [Double])
        let r = try await db.query("get t select meta")
        #expect(r.first?["meta"] == .array([.double(0.1), .double(0.2)]))
        try await db.close()
    }

    @Test func errorsCarryTheirKind() async throws {
        let db = try Fenec.memory()
        try await db.execute("create collection t (a int @unique)")
        try await db.execute("put t {a: 1}")
        let cases: [(String, FenecError.Code)] = [
            ("get nowhere", .notFound),
            ("broken query", .query),
            ("create collection t (a int)", .exists),
            ("put t {a: 1}", .duplicate),
            ("put t {a: \"x\"}", .type),
        ]
        for (text, code) in cases {
            await #expect(throws: FenecError.self) { try await db.run(text) }
            do {
                _ = try await db.run(text)
            } catch let e as FenecError {
                #expect(e.code == code, "\(text): \(e)")
                #expect(!e.message.isEmpty)
            }
        }
        // The builder refuses before anything runs.
        do {
            _ = try db.from("t; drop collection t")
            Issue.record("a name that is no name was taken")
        } catch let e as FenecError {
            #expect(e.code == .builder)
            #expect(e.message == #"invalid collection name: "t; drop collection t""#)
        }
        try await db.close()
        // A closed database refuses, and saying so twice is nothing.
        await #expect(throws: FenecError(code: .misuse, message: "the database was closed")) {
            try await db.run("get t")
        }
        try await db.close()
    }

    @Test func aFileIsThereAgainAndOpenOnce() async throws {
        let path = try scratch("reopen")
        var db = try await Fenec.open(path: path)
        try await db.execute("create collection t (title text, e vector<2> @hnsw(cosine))")
        for i in 0..<20 {
            try await db.execute("put t {title: $1, e: $2}", "n\(i)", [Float(i) / 10, 1])
        }
        await #expect(throws: FenecError.self) { _ = try await Fenec.open(path: path) }
        do {
            _ = try await Fenec.open(path: path)
        } catch let e as FenecError {
            #expect(e.code == .locked)
        }
        try await db.checkpoint()
        try await db.close()
        db = try await Fenec.open(path: path, options: [.noSync])
        #expect(try await db.from("t").count() == 20)
        try await db.execute("put t {title: \"after\"}")
        try await db.flush()
        try await db.sync()
        try await db.close()
        db = try await open(url: path, options: [.inMemory])
        #expect(try await db.from("t").count() == 21)
        let near = try await db.from("t").select("title").near("e", [Float(0), 1]).limit(1).rows()
        #expect(near.first?["title"] == "n0")
        try await db.close()
    }

    @Test func manyTasksShareOneDatabase() async throws {
        let db = try await Fenec.open(path: try scratch("tasks"))
        try await db.execute("create collection t (w int, e vector<2> @hnsw(l2))")
        try await withThrowingTaskGroup(of: Void.self) { group in
            for w in 0..<8 {
                group.addTask {
                    for i in 0..<20 {
                        if w % 2 == 0 {
                            try await db.execute("put t {w: $1, e: $2}", w, [Float(w), Float(i)])
                        } else {
                            _ = try await db.query("get t near e $1 limit 3", [Float(0), 1])
                        }
                    }
                }
            }
            try await group.waitForAll()
        }
        #expect(try await db.from("t").count() == 80)
        try await db.close()
    }

    @Test func valuesWriteAndReadAsJson() throws {
        let v: Value = ["s": "a\"b\n\u{1}ç", "n": -1, "f": 0.5, "ok": true, "none": nil, "list": [1, 2.5]]
        #expect(v.json == #"{"s":"a\"b\n\u0001ç","n":-1,"f":0.5,"ok":true,"none":null,"list":[1,2.5]}"#)
        #expect(try Value.parse(v.json) == v)
        #expect(try Value.parse(#"{"e":"😀","x":1e3}"#)["e"] == "😀")
        // The one Float whose shortest text rounds away through a Double.
        let tie = Float(7.038531e-26)
        #expect(Float(Double(Value.number(tie))!) == tie)
        #expect(Value.number(Float(0.1)) == "0.1")
        #expect(isoText(seconds: 1_789_821_296.789) == "2026-09-19T12:34:56.789Z")
        #expect(isoText(seconds: -0.001) == "1969-12-31T23:59:59.999Z")
        #expect(Fenec.version.split(separator: ".").count == 3)
    }
}
