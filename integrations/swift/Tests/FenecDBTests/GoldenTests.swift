import Testing

@testable import FenecDB

/// The query builder held to integrations/builder-golden.json: the text and
/// parameters the JavaScript builder makes of every chain there, refusals
/// by their message. The endpoints' statements are read off a recording
/// executor. Run against a database too (`theBuilderAnswersAsTheText`).
@Suite struct Golden {
    static let cases: [Value] = {
        let text = goldenText()
        return (try? Value.parse(text))?.array ?? []
    }()

    static var names: [String] { cases.compactMap { $0["name"]?.string } }

    @Test func theFileHoldsEnoughCases() {
        #expect(Golden.cases.count >= 150, "\(Golden.cases.count) cases")
    }

    @Test(arguments: Golden.names)
    func aCase(_ name: String) async throws {
        let c = Golden.cases.first { $0["name"]?.string == name }!
        let (text, params, error) = await Golden.run(c["steps"]!.array!)
        if let want = c["error"]?.string {
            #expect(error == want, "\(name): made \(text ?? "nothing")")
            return
        }
        #expect(error == nil, "\(name)")
        #expect(text == c["text"]?.string, "\(name)")
        #expect(Golden.same(.array(params ?? []), c["params"]!), "\(name): \(Value.array(params ?? []).json)")
    }

    // ------------------------------------------------------------ the steps

    /// An argument as a Swift caller hands it over: `$date` a `Date`, `$f32`
    /// a `[Float]`, an object a `Value` of its fields in order.
    static func value(_ v: Value) -> Value {
        switch v {
        case .object(let r):
            if let d = r["$date"]?.string { return date(d) }
            if let f = r["$f32"]?.array { return .floats(f.map { Float($0.double!) }) }
            if let n = r["$inc"] { return .inc(n) }
            if let e = r["$expr"]?.array { return .expr(e[0].string!, e.dropFirst().map(value)) }
            return .object(Row(zip(r.keys, r.values.map(value)).map { ($0, $1) }))
        case .array(let a): return .array(a.map(value))
        default: return v
        }
    }

    /// A select item or a group key: a name as itself, `$expr`, `$bucket`,
    /// `$countDistinct`, `$first`, `$last` as `Column`'s, `$as` naming one.
    static func column(_ v: Value) throws -> Column {
        if let name = v.string { return Column(name) }
        if let e = v["$expr"]?.array { return .expr(e[0].string!, e.dropFirst().map(value)) }
        if let b = v["$bucket"]?.array { return try .bucket(b[0].string!, b[1].string!) }
        if let f = v["$countDistinct"]?.string { return try .countDistinct(f) }
        if let f = v["$first"]?.array { return try .first(f[0].string!, by: f.count > 1 ? f[1].string! : nil) }
        if let f = v["$last"]?.array { return try .last(f[0].string!, by: f.count > 1 ? f[1].string! : nil) }
        if let a = v["$as"]?.array { return try column(a[0]).as(a[1].string!) }
        Issue.record("no column \(v.json)")
        return Column("")
    }

    static func cond(_ v: Value) -> Cond {
        if let or = v["$or"]?.array { return Cond(.or(or.map(cond))) }
        if let and = v["$and"]?.array { return Cond(.and(and.map(cond))) }
        if let not = v["$not"] { return .not(cond(not)) }
        if let raw = v["$raw"]?.array {
            return Cond(.raw(raw[0].string!, raw.dropFirst().map { value($0) as any FenecValue }))
        }
        let r = v.object!
        return .fields(Row(zip(r.keys, r.values.map(value)).map { ($0, $1) }))
    }

    static func opt(_ args: [Value], _ at: Int, _ name: String) -> Value? {
        args.count > at ? args[at][name] : nil
    }

    static func sortKeys(_ v: Value?) -> [SortKey] {
        guard let v else { return [] }
        if let s = v.string { return [SortKey(s)] }
        return v.array!.map { k in
            if let s = k.string { return SortKey(s) }
            let a = k.array!
            return SortKey(a[0].string!, a.count > 1 ? a[1].string! : "asc", collate: a.count > 2 ? a[2]["collate"]?.string : nil)
        }
    }

    static func step(_ q: Query, _ op: String, _ a: [Value]) throws -> Query {
        switch (op, a.count) {
        case ("select", _): return try q.select(a.map(column))
        case ("where", 3): return try q.where(a[0].string!, a[1].string!, value(a[2]))
        case ("where", 2): return try q.where(a[0].string!, value(a[1]))
        case ("where", _): return try q.where(cond(a[0]))
        case ("orWhere", 3): return try q.orWhere(a[0].string!, a[1].string!, value(a[2]))
        case ("orWhere", 2): return try q.orWhere(a[0].string!, value(a[1]))
        case ("orWhere", _): return try q.orWhere(cond(a[0]))
        case ("near", _):
            return try q.near(a[0].string!, value(a[1]), ef: opt(a, 2, "ef")?.int, exact: opt(a, 2, "exact")?.bool ?? false)
        case ("rerank", _): return try q.rerank(a[0].string!, value(a[1]), candidates: opt(a, 2, "candidates")?.int)
        case ("match", _): return try q.match(a[0].string!, a[1].string!)
        case ("fuse", _): return try q.fuse(k: opt(a, 0, "k")?.int, candidates: opt(a, 0, "candidates")?.int)
        // One key as itself, several (or none) as the list they are.
        case ("group", 1): return try q.group(a[0].array.map { try $0.map(column) } ?? [column(a[0])])
        case ("group", _): return try q.group(a.map(column))
        // The tags and the ellipsis as the file holds them, a number too,
        // so a refusal of one is the JS builder's.
        case ("highlight", _): return try q.mark(highlight: a[0].string!, pre: opt(a, 1, "pre"), post: opt(a, 1, "post"))
        case ("snippet", _):
            return try q.mark(
                snippet: a[0].string!, words: a[1].int!, ellipsis: opt(a, 2, "ellipsis"), pre: opt(a, 2, "pre"),
                post: opt(a, 2, "post"))
        // The options as the file holds them, a bound that is no number too.
        case ("facet", _):
            return try q.facet(
                a[0].string!, top: opt(a, 1, "top")?.int, rangeValues: opt(a, 1, "ranges"),
                disjunctiveValue: opt(a, 1, "disjunctive"))
        case ("order", _):
            return try q.order(a[0].string!, a.count > 1 ? a[1].string! : "asc", collate: opt(a, 2, "collate")?.string)
        case ("limit", _): return try q.limit(a[0].int!)
        case ("offset", _): return try q.offset(a[0].int!)
        case ("require", _): return try q.require(a[0].int!)
        case ("lookup", _):
            let select: [String]? = opt(a, 1, "select").map { $0.string.map { [$0] } ?? $0.array!.map { $0.string! } }
            return try q.lookup(
                a[0].string!, on: opt(a, 1, "on")?.string, parentKey: opt(a, 1, "parentKey")?.string, select: select,
                where: opt(a, 1, "where").map(cond), required: opt(a, 1, "required")?.bool ?? false,
                order: sortKeys(opt(a, 1, "order")), limit: opt(a, 1, "limit")?.int, offset: opt(a, 1, "offset")?.int)
        default:
            Issue.record("no builder step \(op)")
            return q
        }
    }

    /// What the endpoints sent.
    actor Recorder {
        var sent: (String, [Value])?
        func record(_ text: String, _ params: [Value]) { sent = (text, params) }
    }

    static func run(_ steps: [Value]) async -> (String?, [Value]?, String?) {
        let recorder = Recorder()
        do {
            var q = try Query.from(steps[0]["args"]![0]!.string!).bind { text, params in
                await recorder.record(text, params)
                if text.hasPrefix("put ") || text.hasPrefix("set ") || text.hasPrefix("del ") { return .affected(0) }
                if text.hasSuffix(" count") { return .rows(columns: ["count"], rows: [["count": 0]]) }
                return .rows(columns: [], rows: [])
            }
            for s in steps.dropFirst() {
                let op = s["op"]!.string!
                let a = s["args"]?.array ?? []
                let docs = { value(a[0]) }
                let all = { (at: Int) in opt(a, at, "all")?.bool ?? false }
                let absent = { opt(a, 1, "ifAbsent")?.bool ?? false }
                let require = { (at: Int) in opt(a, at, "require")?.int }
                var made: (text: String, params: [Value])?
                switch op {
                case "toFenecQL": made = try q.toFenecQL()
                case "toInsert": made = try q.toInsert(docs(), ifAbsent: absent(), require: require(1))
                case "toUpdate": made = try q.toUpdate(docs(), all: all(1), require: require(1))
                case "toUpsert": made = try q.toUpsert(docs(), value(a[1]), require: require(2))
                case "toDelete": made = try q.toDelete(all: all(0), require: require(0))
                case "rows": _ = try await q.rows()
                case "first": _ = try await q.first()
                case "count": _ = try await q.count()
                case "explain": _ = try await q.explain()
                case "insert": _ = try await q.insert(docs(), ifAbsent: absent(), require: require(1))
                case "update": _ = try await q.update(docs(), all: all(1), require: require(1))
                case "upsert": _ = try await q.upsert(docs(), value(a[1]), require: require(2))
                case "delete": _ = try await q.delete(all: all(0), require: require(0))
                default:
                    q = try step(q, op, a)
                    continue
                }
                if let made { return (made.text, made.params, nil) }
                let sent = await recorder.sent
                return (sent?.0, sent?.1, nil)
            }
            return (nil, nil, "a chain ends with a statement")
        } catch let e as FenecError {
            return (nil, nil, e.message)
        } catch {
            return (nil, nil, "\(error)")
        }
    }

    /// JSON compared by value: a number the file writes as 1 and the builder
    /// holds as 1.0 is one number to the engine.
    static func same(_ a: Value, _ b: Value) -> Bool {
        if let x = a.double, let y = b.double { return x == y }
        if let x = a.array, let y = b.array { return x.count == y.count && zip(x, y).allSatisfy(same) }
        if let x = a.object, let y = b.object {
            return x.count == y.count && x.allSatisfy { k, v in y[k].map { same(v, $0) } ?? false }
        }
        return a == b
    }

    // ------------------------------------------------- against the engine

    @Test func theBuilderAnswersAsTheText() async throws {
        let db = try Fenec.memory()
        try await db.execute(
            "create collection shelf (title text, year int @sorted, lang text @hash, tags [text], body text @text, embed vector<3> @hnsw(cosine)); create collection notes (doc_id int @hash, stars int)"
        )
        let shelf = try db.from("shelf")
        #expect(
            try await shelf.insert([
                ["title": "Night at the oasis", "year": 2024, "lang": "en", "tags": ["desert"],
                 "body": "a night under the stars at the oasis", "embed": .floats([0.1, 0.2, 0.3])] as Value,
                ["title": "Dunes", "year": 2021, "lang": "en", "tags": ["desert", "sand"], "body": "dunes move with the wind",
                 "embed": .floats([0.9, 0.1, 0])],
                ["title": "Kum", "year": 2023, "lang": "tr", "tags": ["sand"], "body": "kum ve rüzgar",
                 "embed": .floats([0.2, 0.8, 0.1])],
            ] as Value) == 3)
        try await db.from("notes").insert([["doc_id": 1, "stars": 5], ["doc_id": 1, "stars": 3], ["doc_id": 3, "stars": 4]] as Value)
        let v: [Float] = [0.1, 0.2, 0.3]
        let pairs: [(Query, String, [any FenecValue])] = [
            (try shelf.select("title").where("year", ">=", 2022).order("year", "desc"),
             "get shelf select title where year >= $1 order year desc", [2022]),
            (try shelf.select("title").where(["lang": "en", "tags": ["has": "sand"]]),
             "get shelf select title where lang = $1 and tags has $2", ["en", "sand"]),
            (try shelf.select("title").where(.or(.cmp("lang", "=", "tr"), .cmp("year", "<", 2022))).order("title"),
             "get shelf select title where lang = $1 or year < $2 order title asc", ["tr", 2022]),
            (try shelf.select("title").near("embed", v).limit(2), "get shelf select title near embed $1 limit 2", [v]),
            (try shelf.select("title").match("body", "oasis stars"), "get shelf select title match body $1", ["oasis stars"]),
            (try shelf.select("title").where("id", "in", [1, 3]).lookup("notes", on: "doc_id", select: ["stars"], order: [SortKey("stars", "desc")]),
             "get shelf select title where id in [$1, $2] lookup notes on doc_id select stars order stars desc", [1, 3]),
            (try shelf.select("lang", "count(*)").group("lang").order("lang"),
             "get shelf select lang, count(*) group lang order lang asc", []),
        ]
        for (q, text, params) in pairs {
            #expect(try q.toFenecQL().text == text)
            let got = try await q.rows()
            #expect(!got.isEmpty, "\(text)")
            #expect(got == (try await db.run(text, params: params.map { try $0.fenecValue() }).rows), "\(text)")
        }
        #expect(try await shelf.where("lang", "en").count() == 2)
        #expect(try await shelf.order("year").first()?["title"] == "Dunes")
        #expect(try await shelf.where("lang", "xx").first() == nil)
        #expect(!(try await shelf.near("embed", v).limit(1).explain()).isEmpty)
        #expect(try await shelf.where("lang", "tr").update(["year": 2025] as Value) == 1)
        await #expect(throws: FenecError.self) { try await shelf.delete() }
        #expect(try await shelf.where("year", "<", 2022).delete() == 1)
        #expect(try await shelf.delete(all: true) == 2)
        #expect(try await shelf.count() == 0)
        try await db.close()
    }

    @Test func aQueryIsAValueToBranchFrom() throws {
        let b = try Query.from("articles").where("year", ">=", 2024)
        #expect(try b.toFenecQL().text == "get articles where year >= $1")
        #expect(try b.where("tags", "has", "rust").toFenecQL().text == "get articles where year >= $1 and tags has $2")
        #expect(try b.limit(3).toFenecQL().text == "get articles where year >= $1 limit 3")
        #expect(b.reads == ["articles"])
        #expect(try b.lookup("notes", on: "article_id").reads == ["articles", "notes"])
        #expect(try b.where(.raw("id in (get x select id)")).reads == nil)
    }
}
