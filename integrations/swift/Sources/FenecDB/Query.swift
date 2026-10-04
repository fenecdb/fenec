import Foundation

// The query builder: FenecQL text and its parameters from a chain of calls.
//
//     let rows = try await db.from("docs")
//         .select("title")
//         .where("year", ">=", 2024)
//         .near("embed", vector, ef: 64)
//         .limit(5)
//         .rows()
//
// It makes the text web/fenec.js's builder makes of the same chain, to the
// byte, as the Python, Go, .NET, Kotlin and Dart builders do:
// integrations/builder-golden.json holds the chains and what each must make,
// and every builder's tests run it. Every value goes in as a parameter; a
// name -- a collection, a field, a path into a json field -- cannot, so
// names are checked against FenecQL's own rule, and that check is the
// injection boundary. A step the builder refuses throws as it is called
// (`FenecError.Code.builder`), its message the JS builder's.

/// A condition: what `or`, `and`, `not`, `raw` and `cmp` make, or the JS
/// builder's object condition written as a dictionary literal --
/// `["lang": "en", "year": ["gte": 2024]]` -- each field's value equality,
/// `nil` (`.null`) `is null`, and an object its operators, in the order
/// written, which is the text's.
public struct Cond: Sendable, ExpressibleByDictionaryLiteral {
    indirect enum Spec: Sendable {
        case or([Cond]), and([Cond]), not(Cond)
        case raw(String, [any FenecValue])
        case cmp(String, String, any FenecValue)
        case fields([(String, Value)])
    }

    let spec: Spec

    init(_ spec: Spec) { self.spec = spec }

    public init(dictionaryLiteral elements: (String, Value)...) { spec = .fields(elements) }

    /// The fields' conditions, joined with `and`.
    public static func fields(_ row: Row) -> Cond { Cond(.fields(Array(zip(row.keys, row.values)))) }

    /// Joins conditions with `or`.
    public static func or(_ conds: Cond...) -> Cond { Cond(.or(conds)) }

    /// Joins conditions with `and`: `where` already ands, so this is only
    /// needed inside `or`.
    public static func and(_ conds: Cond...) -> Cond { Cond(.and(conds)) }

    /// Negates a condition.
    public static func not(_ cond: Cond) -> Cond { Cond(.not(cond)) }

    /// What the builder cannot express (a function call): each `?` is bound
    /// to the next parameter -- a literal `?` goes in as one too.
    /// `.raw("cosine(embed, ?) > ?", vector, 0.5)`
    public static func raw(_ sql: String, _ params: any FenecValue...) -> Cond { Cond(.raw(sql, params)) }

    /// One comparison, `where`'s three arguments as a condition.
    public static func cmp(_ field: String, _ op: String, _ value: any FenecValue) -> Cond {
        Cond(.cmp(field, op, value))
    }
}

// A value a write works out over the row it writes, as the JS builder's
// `inc` and `expr`: an object under a key no field can be named (a NUL
// opens it), so that it rides in a document's `Value` as any value does and
// the builder renders it as FenecQL, its values as parameters.
extension Value {
    static let computedKey = "\u{0}fenec.computed"

    /// `["n": .inc(1)]` in an update: the field plus `by`, counting from 0
    /// where it is null -- `n: coalesce(n, 0) + $1` -- worked out under the
    /// write lock, so increments from many clients all land.
    public static func inc(_ by: Value = 1) -> Value {
        .object(Row([(computedKey, "inc"), ("by", by)]))
    }

    /// A value as a FenecQL expression over the row, each `?` bound to the
    /// next parameter: `["at": .expr("now()")]`, `.expr("price * ?", 1.2)`.
    public static func expr(_ sql: String, _ params: Value...) -> Value { expr(sql, params) }

    static func expr(_ sql: String, _ params: [Value]) -> Value {
        .object(Row([(computedKey, "expr"), ("sql", .string(sql)), ("params", .array(params))]))
    }
}

/// One key of a lookup's order: a field, `asc` or `desc`, and a collation
/// (`tr` or `und`). A string literal is an ascending key.
public struct SortKey: Sendable, ExpressibleByStringLiteral {
    public let field: String
    public let direction: String
    public let collate: String?

    public init(_ field: String, _ direction: String = "asc", collate: String? = nil) {
        self.field = field
        self.direction = direction
        self.collate = collate
    }

    public init(stringLiteral value: String) { self.init(value) }
}

indirect enum Node: Sendable {
    case and([Node]), or([Node]), not(Node)
    case null(String, negated: Bool)
    case `in`(String, [Value])
    case cmp(String, String, Value)
    case raw(String, [Value])
}

enum Builder {
    static let maxLookupDepth = 8

    static let ops: [String: String] = [
        "=": "=", "eq": "=",
        "!=": "!=", "ne": "!=", "neq": "!=",
        "<": "<", "lt": "<",
        "<=": "<=", "lte": "<=", "le": "<=",
        ">": ">", "gt": ">",
        ">=": ">=", "gte": ">=", "ge": ">=",
        "~": "~", "like": "~", "contains": "~",
        "has": "has",
        "in": "in",
    ]

    /// FenecQL's identifier: a Unicode letter or `_`, then letters, digits
    /// and `_`, as the lexer reads it -- JavaScript's `\p{Alphabetic}` and
    /// `\p{N}`, which Swift's scalar properties name the same.
    static func isIdent<S: StringProtocol>(_ s: S) -> Bool {
        var first = true
        for u in s.unicodeScalars {
            let p = u.properties
            let start = u == "_" || p.isAlphabetic
            let more: Bool
            switch p.generalCategory {
            case .decimalNumber, .letterNumber, .otherNumber: more = true
            default: more = false
            }
            if !(start || (!first && more)) { return false }
            first = false
        }
        return !first
    }

    static func ident(_ name: String, _ what: String = "field") throws -> String {
        guard isIdent(name) else { throw FenecError.builder("invalid \(what) name: \(quote(name))") }
        return name
    }

    /// A field's name, or a path into a json field: `meta.lang`.
    static func path(_ name: String) throws -> String {
        guard name.split(separator: ".", omittingEmptySubsequences: false).allSatisfy(isIdent) else {
            throw FenecError.builder("invalid field name: \(quote(name))")
        }
        return name
    }

    /// A name as `JSON.stringify` writes it, which the messages quote with.
    static func quote(_ s: String) -> String {
        var out = ""
        Value.quote(s, into: &out)
        return out
    }

    /// What JavaScript's `String.prototype.trim` takes off.
    static func jsSpace(_ u: Unicode.Scalar) -> Bool {
        switch u.value {
        case 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x20, 0xa0, 0x1680, 0x2028, 0x2029, 0x202f, 0x205f, 0x3000, 0xfeff: return true
        case 0x2000...0x200a: return true
        default: return false
        }
    }

    static func asciiIdent(_ s: Substring) -> Bool {
        guard let f = s.unicodeScalars.first, f.isASCII, f.properties.isAlphabetic || f == "_" else { return false }
        return s.unicodeScalars.allSatisfy { $0.isASCII && ($0.properties.isAlphabetic || ("0"..."9").contains($0) || $0 == "_") }
    }

    /// A select item: a field, or an aggregate spelled as FenecQL spells it
    /// -- `count(*)`, `sum(total)`, `avg(f)`, `min(f)`, `max(f)` -- answering
    /// under that name. Read by hand rather than by a case-blind pattern,
    /// which would fold the Kelvin sign onto `k` where the JS builder's does
    /// not.
    static func column(_ name: String) throws -> (text: String, aggregate: Bool) {
        var scalars = Substring(name).unicodeScalars[...]
        while let f = scalars.first, jsSpace(f) { scalars.removeFirst() }
        while let l = scalars.last, jsSpace(l) { scalars.removeLast() }
        let s = Substring(scalars)
        if let open = s.firstIndex(of: "("), open > s.startIndex, s.hasSuffix(")") {
            let fn = s[s.startIndex..<open]
            let arg = s[s.index(after: open)..<s.index(before: s.endIndex)]
            let low = fn.unicodeScalars.allSatisfy({ $0.isASCII && $0.properties.isAlphabetic }) ? fn.lowercased() : nil
            if low == "count", arg.isEmpty || arg == "*" { return ("count(*)", true) }
            if let low, ["sum", "avg", "min", "max"].contains(low), asciiIdent(arg) { return ("\(low)(\(arg))", true) }
        }
        return (try path(name), false)
    }

    static func direction(_ dir: String) throws -> Bool {
        switch dir.lowercased() {
        case "asc": return true
        case "desc": return false
        default: throw FenecError.builder("order direction must be 'asc' or 'desc': \(dir)")
        }
    }

    /// The collations the engine knows. The name is spliced into the text,
    /// so it is checked against the list rather than the name pattern.
    static func collation(_ name: String?) throws -> String? {
        guard let name else { return nil }
        guard name == "und" || name == "tr" else {
            throw FenecError.builder("unknown collation: \(quote(name)); there are 'und' and 'tr'")
        }
        return name
    }

    /// `limit`, `offset`, `ef` and the rest are literals in FenecQL, never
    /// parameters: a whole number JavaScript holds exactly.
    static func whole(_ n: Int, _ what: String) throws -> Int {
        guard n >= 0, n <= (1 << 53) - 1 else { throw FenecError.builder("\(what) must be a non-negative integer: \(n)") }
        return n
    }

    /// A mark's tags: both or neither, each text.
    static func tags(_ pre: Value?, _ post: Value?, _ what: String) throws -> (pre: String, post: String)? {
        guard pre != nil || post != nil else { return nil }
        guard let pre, let post else { throw FenecError.builder("\(what) takes both pre and post, or neither") }
        return (try text(pre, "\(what) pre"), try text(post, "\(what) post"))
    }

    static func text(_ v: Value, _ what: String) throws -> String {
        guard let s = v.string else { throw FenecError.builder("\(what) must be text: \(v.json)") }
        return s
    }

    static func node(_ c: Cond) throws -> Node {
        switch c.spec {
        case .or(let items): return .or(try items.map(node))
        case .and(let items): return .and(try items.map(node))
        case .not(let item): return .not(try node(item))
        case .raw(let sql, let params): return .raw(sql, try params.map { try $0.fenecValue() })
        case .cmp(let field, let op, let value): return try condOf(field, op, value.fenecValue())
        case .fields(let fields):
            let items = try fields.map { try fieldCond(try path($0.0), $0.1) }
            return items.count == 1 ? items[0] : .and(items)
        }
    }

    static func condOf(_ field: String, _ op: String, _ value: Value) throws -> Node {
        guard let o = ops[op] else { throw FenecError.builder("unknown operator `\(op)`") }
        let f = try path(field)
        return o == "in" ? try inCond(f, value) : try cmp(f, o, value)
    }

    /// One field's condition: a value is equality, `.null` is null, an
    /// object its operators.
    static func fieldCond(_ field: String, _ spec: Value) throws -> Node {
        guard case .object(let opsMap) = spec else {
            return spec.isNull ? .null(field, negated: false) : try cmp(field, "=", spec)
        }
        var items: [Node] = []
        for (k, v) in opsMap {
            if k == "not" {
                items.append(v.isNull ? .null(field, negated: true) : .not(try fieldCond(field, v)))
                continue
            }
            guard let op = ops[k] else { throw FenecError.builder("unknown operator `\(k)` (field: \(field))") }
            items.append(op == "in" ? try inCond(field, v) : try cmp(field, op, v))
        }
        switch items.count {
        case 0: throw FenecError.builder("empty condition object (field: \(field))")
        case 1: return items[0]
        default: return .and(items)
        }
    }

    static func inCond(_ field: String, _ values: Value) throws -> Node {
        guard case .array(let items) = values else {
            throw FenecError.builder("`in` expects an array (field: \(field))")
        }
        guard !items.isEmpty else { throw FenecError.builder("`in` does not accept an empty array (field: \(field))") }
        return .in(field, items)
    }

    /// `= null` is never true in FenecQL; what is meant is `is null`.
    static func cmp(_ field: String, _ op: String, _ value: Value) throws -> Node {
        guard value.isNull else { return .cmp(field, op, value) }
        switch op {
        case "=": return .null(field, negated: false)
        case "!=": return .null(field, negated: true)
        default: throw FenecError.builder("`\(op)` cannot be used with null (field: \(field))")
        }
    }

    /// Flattens empty and single-child junctions before rendering: the
    /// parentheses depend on the child count, and rendering binds parameters.
    static func prune(_ c: Node) -> Node? {
        switch c {
        case .and(let items), .or(let items):
            let kept = items.compactMap(prune)
            if kept.isEmpty { return nil }
            if kept.count == 1 { return kept[0] }
            if case .and = c { return .and(kept) }
            return .or(kept)
        case .not(let item):
            return prune(item).map { .not($0) }
        default: return c
        }
    }

    static func render(_ c: Node, _ bind: Binder, parent: String? = nil) throws -> String {
        switch c {
        case .and(let items), .or(let items):
            let word: String
            if case .and = c { word = "and" } else { word = "or" }
            let s = try items.map { try render($0, bind, parent: word) }.joined(separator: " \(word) ")
            // `and` binds tighter than `or`: one inside the other needs parens.
            if let parent, parent != word { return "(\(s))" }
            return s
        case .not(let item): return "not (\(try render(item, bind)))"
        case .null(let field, let negated): return "\(field) is \(negated ? "not " : "")null"
        case .in(let field, let values): return "\(field) in [\(values.map(bind.bind).joined(separator: ", "))]"
        case .cmp(let field, let op, let value): return "\(field) \(op) \(bind.bind(value))"
        case .raw(let sql, let params):
            let pieces = sql.split(separator: "?", omittingEmptySubsequences: false)
            var out = ""
            for i in 0..<pieces.count - 1 {
                guard i < params.count else { throw FenecError.builder("raw(): more `?` placeholders than parameters") }
                out += pieces[i] + bind.bind(params[i])
            }
            guard pieces.count - 1 == params.count else { throw FenecError.builder("raw(): too many parameters given") }
            return out + pieces[pieces.count - 1]
        }
    }

    /// A document's fields, in order; `insert` refuses an `inc`, which reads
    /// the row it changes.
    static func renderDoc(_ doc: Value, _ bind: Binder, insert: Bool = false) throws -> String {
        guard case .object(let row) = doc else { throw FenecError.builder("expected a document object") }
        guard !row.isEmpty else { throw FenecError.builder("cannot write an empty document") }
        return "{" + (try row.map {
            let key = try path($0.key)
            return "\(key): \(try value(key, $0.value, bind, insert))"
        }).joined(separator: ", ") + "}"
    }

    /// A document's value: `inc`'s and `expr`'s text, or a parameter.
    static func value(_ key: String, _ v: Value, _ bind: Binder, _ insert: Bool) throws -> String {
        guard case .object(let r) = v, let kind = r[Value.computedKey]?.string else { return bind.bind(v) }
        if kind == "inc" {
            let by = r["by"] ?? .null
            let finite: Bool
            switch by {
            case .int: finite = true
            case .double(let d): finite = d.isFinite
            default: finite = false
            }
            guard finite else { throw FenecError.builder("inc() takes a number: \(by.json)") }
            if insert {
                throw FenecError.builder("inc() reads the row it changes: use it in update (field: \(key))")
            }
            return "coalesce(\(key), 0) + \(bind.bind(by))"
        }
        let sql = r["sql"]?.string ?? ""
        let params = r["params"]?.array ?? []
        let pieces = sql.split(separator: "?", omittingEmptySubsequences: false)
        var out = ""
        for i in 0..<pieces.count - 1 {
            guard i < params.count else { throw FenecError.builder("expr(): more `?` placeholders than parameters") }
            out += pieces[i] + bind.bind(params[i])
        }
        guard pieces.count - 1 == params.count else { throw FenecError.builder("expr(): too many parameters given") }
        return out + pieces[pieces.count - 1]
    }

    /// Whether a `raw` fragment may read a collection of its own.
    static func readsMore(_ c: Node) -> Bool {
        switch c {
        case .and(let items), .or(let items): return items.contains(where: readsMore)
        case .not(let item): return readsMore(item)
        case .raw(let sql, _): return sql.range(of: #"\bget\b"#, options: [.regularExpression, .caseInsensitive]) != nil
        default: return false
        }
    }
}

final class Binder {
    private(set) var params: [Value] = []

    func bind(_ v: Value) -> String {
        params.append(v)
        return "$\(params.count)"
    }
}

/// A query over one collection, made by `Fenec.from` or `Query.from`.
/// Immutable: each call hands back a new one, so a base query can be kept
/// and branched from, from several tasks too.
public struct Query: Sendable {
    struct VectorClause: Sendable {
        let field: String
        let vector: Value
        let n: Int?
        let exact: Bool
    }

    struct Level: Sendable {
        let collection: String
        let on: String
        let parent: String?
        let project: [String]?
        let cond: [Node]
        let required: Bool
        let order: [(field: String, asc: Bool, collate: String?)]
        let limit: Int?
        let offset: Int
    }

    /// `highlight(field)` when `words` is nil, `snippet(field, words)`
    /// otherwise; the tags both or neither.
    struct Mark: Sendable {
        let field: String
        let words: Int?
        let ellipsis: String?
        let tags: (pre: String, post: String)?

        var kind: String { words == nil ? "highlight" : "snippet" }
    }

    typealias Exec = @Sendable (String, [Value]) async throws -> Answer

    public let collection: String
    var project: [String]?
    var aggregate = false
    var group: String?
    var cond: [Node] = []
    var near: VectorClause?
    var match: (field: String, query: Value)?
    var rerank: VectorClause?
    var fuse: (k: Int?, candidates: Int?)?
    var order: [(field: String, asc: Bool, collate: String?)] = []
    var limit: Int?
    var offset = 0
    var count = false
    var lookups: [Level] = []
    var marks: [Mark] = []
    var facets: [(field: String, top: Int?)] = []
    var exec: Exec?

    init(collection: String) { self.collection = collection }

    /// A query bound to no database: for its text alone, `toFenecQL()`.
    public static func from(_ collection: String) throws -> Query {
        Query(collection: try Builder.ident(collection, "collection"))
    }

    /// The query bound to whatever runs a text and its parameters.
    public func bind(_ exec: @escaping @Sendable (String, [Value]) async throws -> Answer) -> Query {
        var q = self
        q.exec = exec
        return q
    }

    private func with(_ change: (inout Query) throws -> Void) rethrows -> Query {
        var q = self
        try change(&q)
        return q
    }

    /// Every collection the query reads -- its own and each lookup's -- or
    /// `nil` when a `raw` fragment may read one more: a live query runs
    /// again when one of them is written.
    public var reads: [String]? {
        if cond.contains(where: Builder.readsMore) || lookups.contains(where: { $0.cond.contains(where: Builder.readsMore) }) {
            return nil
        }
        var out = [collection]
        for l in lookups where !out.contains(l.collection) { out.append(l.collection) }
        return out
    }

    /// `select a, b`; none, or `"*"`, is every field. Aggregates go in the
    /// same list as FenecQL spells them and answer under that name:
    /// `.select("status", "count(*)", "sum(total)").group("status")`.
    public func select(_ columns: String...) throws -> Query { try select(columns) }

    public func select(_ columns: [String]) throws -> Query {
        if columns.isEmpty || columns.contains("*") {
            return with { $0.project = nil; $0.aggregate = false }
        }
        let cs = try columns.map(Builder.column)
        return with { $0.project = cs.map(\.text); $0.aggregate = cs.contains { $0.aggregate } }
    }

    /// `group field`: a row per value, for a select list that aggregates.
    public func group(_ field: String) throws -> Query {
        let f = try Builder.ident(field)
        return with { $0.group = f }
    }

    /// `highlight(field)` in the select list: where the terms `match` found
    /// stand in the field's text -- `[start, end]` pairs of UTF-16 offsets,
    /// which `NSRange` and `String.UTF16View` take -- or, given `pre` and
    /// `post`, the text with each mark between them. The text is not
    /// escaped. Answers under `highlight(field)`, after the fields `select`
    /// named.
    ///
    ///     db.from("docs").select("title").highlight("body", pre: "<b>", post: "</b>").match("body", text)
    public func highlight(_ field: String, pre: String? = nil, post: String? = nil) throws -> Query {
        try mark(highlight: field, pre: pre.map(Value.string), post: post.map(Value.string))
    }

    /// `snippet(field, words)`: the window of `words` words around the
    /// densest marks, `{"text": ..., "marks": [[start, end], ...]}` -- or the
    /// marked text, given `pre` and `post` -- with `ellipsis` where it leaves
    /// text out. Answers under `snippet(field)`.
    public func snippet(
        _ field: String, _ words: Int, ellipsis: String? = nil, pre: String? = nil, post: String? = nil
    ) throws -> Query {
        try mark(
            snippet: field, words: words, ellipsis: ellipsis.map(Value.string), pre: pre.map(Value.string),
            post: post.map(Value.string))
    }

    // The tags and the ellipsis as values, so the golden runner can hand
    // over what JavaScript's callers can and be refused by the same message.
    func mark(highlight field: String, pre: Value?, post: Value?) throws -> Query {
        try add(Mark(field: try Builder.ident(field), words: nil, ellipsis: nil, tags: try Builder.tags(pre, post, "highlight")))
    }

    func mark(snippet field: String, words: Int, ellipsis: Value?, pre: Value?, post: Value?) throws -> Query {
        let f = try Builder.ident(field)
        let n = try Builder.whole(words, "snippet words")
        let tags = try Builder.tags(pre, post, "snippet")
        if n == 0 { throw FenecError.builder("snippet shows at least one word") }
        let e = try ellipsis.map { try Builder.text($0, "snippet ellipsis") }
        return try add(Mark(field: f, words: n, ellipsis: e, tags: tags))
    }

    private func add(_ mark: Mark) throws -> Query {
        // Each answers under its label, and a row holds a name once.
        if marks.contains(where: { $0.kind == mark.kind && $0.field == mark.field }) {
            throw FenecError.builder("\(mark.kind)(\(mark.field)) is asked twice")
        }
        return with { $0.marks.append(mark) }
    }

    /// `facet field [top N]`: each value the field -- or a path into a json
    /// field -- holds over every row the query matches, not only the page,
    /// and how many rows hold it, most first; `top` keeps the commonest. A
    /// list counts once a row for each value. The counts come back beside
    /// the rows: `answer().facets`.
    ///
    ///     db.from("products").match("title", "phone").facet("brand", top: 10).facet("color").limit(20)
    public func facet(_ field: String, top: Int? = nil) throws -> Query {
        let f = try Builder.path(field)
        let t = try top.map { try Builder.whole($0, "facet top") }
        if t == 0 { throw FenecError.builder("facet \(f) top 0 answers nothing") }
        if facets.contains(where: { $0.field == f }) { throw FenecError.builder("facet \(f) is asked twice") }
        return with { $0.facets.append((f, t)) }
    }

    /// `field op value`, joined to the conditions before with `and`. The op
    /// is a symbol or its word: `=`, `!=`, `<`, `<=`, `>`, `>=`, `~`, `has`,
    /// `in` (a list), or `eq`, `ne`, `lt`, `gte`, `like`, `contains` ... A
    /// `.null` with `=` or `!=` is `is null` and `is not null`.
    public func `where`(_ field: String, _ op: String, _ value: any FenecValue) throws -> Query {
        let n = try Builder.condOf(field, op, value.fenecValue())
        return with { $0.cond.append(n) }
    }

    /// The field held to a spec: a value is equality, `.null` is null, an
    /// object (a `Value` dictionary literal) its operators.
    public func `where`(_ field: String, _ spec: any FenecValue) throws -> Query {
        let n = try Builder.fieldCond(try Builder.path(field), spec.fenecValue())
        return with { $0.cond.append(n) }
    }

    /// A condition `Cond` made, or a dictionary literal of fields.
    public func `where`(_ cond: Cond) throws -> Query {
        let n = try Builder.node(cond)
        return with { $0.cond.append(n) }
    }

    /// Everything conditioned so far, or `field op value`.
    public func orWhere(_ field: String, _ op: String, _ value: any FenecValue) throws -> Query {
        or(try Builder.condOf(field, op, value.fenecValue()))
    }

    /// Everything conditioned so far, or the field held to a spec.
    public func orWhere(_ field: String, _ spec: any FenecValue) throws -> Query {
        or(try Builder.fieldCond(try Builder.path(field), spec.fenecValue()))
    }

    /// Everything conditioned so far, or the condition.
    public func orWhere(_ cond: Cond) throws -> Query { or(try Builder.node(cond)) }

    private func or(_ right: Node) -> Query {
        with { q in q.cond = q.cond.isEmpty ? [right] : [.or([.and(q.cond), right])] }
    }

    /// `near field $n [ef N] [exact]`. A `[Float]` goes to the library as
    /// its bytes.
    public func near(_ field: String, _ vector: any FenecValue, ef: Int? = nil, exact: Bool = false) throws -> Query {
        let clause = VectorClause(
            field: try Builder.ident(field), vector: try vector.fenecValue(),
            n: try ef.map { try Builder.whole($0, "ef") }, exact: exact)
        return with { $0.near = clause }
    }

    /// `match field $n`: BM25 over a `@text` index.
    public func match(_ field: String, _ query: String) throws -> Query {
        let f = try Builder.ident(field)
        return with { $0.match = (f, .string(query)) }
    }

    /// `fuse [k N] [candidates N]`: with both `match` and `near`, ranks by
    /// both, a document scoring `1 / (k + rank)` from each list it is on.
    public func fuse(k: Int? = nil, candidates: Int? = nil) throws -> Query {
        let f = (try k.map { try Builder.whole($0, "k") }, try candidates.map { try Builder.whole($0, "candidates") })
        return with { $0.fuse = f }
    }

    /// `rerank field $n [candidates N]`: reorders what `match` found by exact
    /// distance, the vectors read out of the store.
    public func rerank(_ field: String, _ vector: any FenecValue, candidates: Int? = nil) throws -> Query {
        let clause = VectorClause(
            field: try Builder.ident(field), vector: try vector.fenecValue(),
            n: try candidates.map { try Builder.whole($0, "candidates") }, exact: false)
        return with { $0.rerank = clause }
    }

    /// `lookup name on child [= parent] ...`: each row's children, attached
    /// to it. `on` names the child's field, `parentKey` the parent's (`id`
    /// unless given); the rest binds to the looked-up collection, and
    /// `limit` counts children per parent. `required` drops a parent no
    /// child matches. Called again, it chains onto the collection the call
    /// before named.
    public func lookup(
        _ name: String, on: String?, parentKey: String? = nil, select: [String]? = nil, where cond: Cond? = nil,
        required: Bool = false, order: [SortKey] = [], limit: Int? = nil, offset: Int? = nil
    ) throws -> Query {
        guard let on, !on.isEmpty else { throw FenecError.builder("lookup needs `on`: the child field holding the key") }
        let level = Level(
            collection: try Builder.ident(name, "collection"),
            on: try Builder.ident(on),
            parent: try parentKey.map { try Builder.ident($0) },
            project: select == nil || select!.contains("*") ? nil : try select!.map(Builder.path),
            cond: try cond.map { [try Builder.node($0)] } ?? [],
            required: required,
            order: try order.map { (try Builder.path($0.field), try Builder.direction($0.direction), try Builder.collation($0.collate)) },
            limit: try limit.map { try Builder.whole($0, "limit") },
            offset: try offset.map { try Builder.whole($0, "offset") } ?? 0)
        return with { $0.lookups.append(level) }
    }

    /// `order field asc|desc`: each call adds a key. `collate: "tr"` puts
    /// text in Turkish order, `"und"` in Unicode's root order.
    public func order(_ field: String, _ direction: String = "asc", collate: String? = nil) throws -> Query {
        // Over groups a key may be an aggregate of the list, by its name.
        let key = (try Builder.column(field).text, try Builder.direction(direction), try Builder.collation(collate))
        return with { $0.order.append(key) }
    }

    /// How many rows come back.
    public func limit(_ n: Int) throws -> Query {
        let n = try Builder.whole(n, "limit")
        return with { $0.limit = n }
    }

    /// How many rows are passed over first.
    public func offset(_ n: Int) throws -> Query {
        let n = try Builder.whole(n, "offset")
        return with { $0.offset = n }
    }

    // ------------------------------------------------------------- the text

    /// The statement and its parameters, as they would be run.
    public func toFenecQL() throws -> (text: String, params: [Value]) {
        let bind = Binder()
        return (try text(bind), bind.params)
    }

    private func text(_ bind: Binder) throws -> String {
        // The engine refuses each of these too; failing here runs nothing.
        if let group, !aggregate {
            throw FenecError.builder("group \(group) needs an aggregate in select: 'count(*)'")
        }
        if aggregate {
            let clash = near != nil ? "near" : match != nil ? "match" : !lookups.isEmpty ? "lookup" : count ? "count" : nil
            if let clash { throw FenecError.builder("aggregates cannot be combined with \(clash)") }
            if group == nil, !order.isEmpty || limit != nil || offset > 0 {
                throw FenecError.builder("aggregates answer one row; group makes a row per value")
            }
        }
        if let first = marks.first {
            if match == nil { throw FenecError.builder("\(first.kind) needs match: it marks the terms match found") }
            if aggregate { throw FenecError.builder("\(first.kind) marks a row's text; aggregates answer groups") }
        }
        if !facets.isEmpty {
            if near != nil {
                throw FenecError.builder(
                    "facet counts the rows a filter or match selects, and near ranks every row: ask the facets without near")
            }
            if aggregate { throw FenecError.builder("facet cannot be combined with aggregates: group counts by value") }
        }
        if rerank != nil, match == nil { throw FenecError.builder("rerank needs match: it reorders what match found") }
        if match != nil, near != nil, fuse == nil {
            throw FenecError.builder("match and near cannot be combined: both order the result; fuse() ranks by both")
        }
        if fuse != nil, match == nil || near == nil {
            throw FenecError.builder("fuse combines match and near: the query needs both")
        }
        if fuse != nil, rerank != nil {
            throw FenecError.builder("fuse and rerank are two ways to use a vector with match: pick one")
        }
        if !lookups.isEmpty {
            let clash = near != nil ? "near" : match != nil ? "match" : rerank != nil ? "rerank" : nil
            if let clash { throw FenecError.builder("lookup cannot be combined with \(clash)") }
            if count, !lookups[0].required {
                throw FenecError.builder(
                    "count cannot be used with lookup unless it is required: there is nothing to attach children to")
            }
            if lookups.count > Builder.maxLookupDepth {
                throw FenecError.builder("lookup chained too deep: at most \(Builder.maxLookupDepth) levels")
            }
            var seen = [collection]
            for l in lookups {
                if seen.contains(l.collection) {
                    throw FenecError.builder("\(l.collection) cannot look itself up: both sides would answer to the same name")
                }
                seen.append(l.collection)
            }
        }
        if count, let extra = extraClause { throw FenecError.builder("count cannot be used with `\(extra)`") }

        var sql = "get \(collection)"
        // The marks after the fields `select` named, or after every field;
        // their tags bound first, being first in the text.
        let items = marks.map { m in
            var s = "\(m.kind)(\(m.field)"
            if let n = m.words { s += ", \(n)" }
            if m.ellipsis != nil || (m.words != nil && m.tags != nil) { s += ", \(bind.bind(.string(m.ellipsis ?? "")))" }
            if let t = m.tags { s += ", \(bind.bind(.string(t.pre))), \(bind.bind(.string(t.post)))" }
            return s + ")"
        }
        if project != nil || !items.isEmpty { sql += " select " + ((project ?? ["*"]) + items).joined(separator: ", ") }
        if let w = try whereOf(cond, bind) { sql += " where \(w)" }
        if let group { sql += " group \(group)" }
        if let near {
            sql += " near \(near.field) \(bind.bind(near.vector))"
            if let ef = near.n { sql += " ef \(ef)" }
            if near.exact { sql += " exact" }
        }
        if let match { sql += " match \(match.field) \(bind.bind(match.query))" }
        if let rerank {
            sql += " rerank \(rerank.field) \(bind.bind(rerank.vector))"
            if let c = rerank.n { sql += " candidates \(c)" }
        }
        if let fuse {
            sql += " fuse"
            if let k = fuse.k { sql += " k \(k)" }
            if let c = fuse.candidates { sql += " candidates \(c)" }
        }
        sql += Query.orderText(order)
        if let limit { sql += " limit \(limit)" }
        if offset > 0 { sql += " offset \(offset)" }
        if count { sql += " count" }
        if !facets.isEmpty {
            sql += " facet " + facets.map { f in f.top.map { "\(f.field) top \($0)" } ?? f.field }.joined(separator: ", ")
        }
        // Terminal, so every clause after it is the child's -- and last, so
        // its parameters come after the parent's.
        for l in lookups {
            sql += " lookup \(l.collection) on \(l.on)"
            if let p = l.parent { sql += " = \(p)" }
            if l.required { sql += " required" }
            if let project = l.project { sql += " select " + project.joined(separator: ", ") }
            if let w = try whereOf(l.cond, bind) { sql += " where \(w)" }
            sql += Query.orderText(l.order)
            if let n = l.limit { sql += " limit \(n)" }
            if l.offset > 0 { sql += " offset \(l.offset)" }
        }
        return sql
    }

    private static func orderText(_ keys: [(field: String, asc: Bool, collate: String?)]) -> String {
        var out = ""
        for (i, k) in keys.enumerated() {
            out += (i == 0 ? " order " : ", ") + k.field
            if let c = k.collate { out += " collate \(c)" }
            out += k.asc ? " asc" : " desc"
        }
        return out
    }

    private func whereOf(_ cond: [Node], _ bind: Binder) throws -> String? {
        guard let root = Builder.prune(.and(cond)) else { return nil }
        return try Builder.render(root, bind)
    }

    private var extraClause: String? {
        near != nil ? "near"
            : match != nil ? "match"
            : rerank != nil ? "rerank"
            : !order.isEmpty ? "order"
            : limit != nil ? "limit"
            : offset > 0 ? "offset"
            : project != nil ? "select"
            : nil
    }

    // Near, order, limit mean something only to a read; dropped from a
    // write, `limit(1).delete()` would delete every row.
    private func assertPlain(_ verb: String) throws {
        if let extra = extraClause { throw FenecError.builder("\(verb) cannot be used with `\(extra)`") }
        if !lookups.isEmpty { throw FenecError.builder("\(verb) cannot be used with `lookup`") }
        if !facets.isEmpty { throw FenecError.builder("\(verb) cannot be used with `facet`") }
        if verb == "insert", !cond.isEmpty { throw FenecError.builder("insert cannot be used with `where`") }
    }

    // An update or delete of every row is too easy to do by accident and
    // cannot be undone: it has to be asked for, with `all`.
    private func requireFilter(_ verb: String, _ all: Bool, _ bind: Binder) throws -> String {
        if let w = try whereOf(cond, bind) { return " where \(w)" }
        if all { return "" }
        throw FenecError.builder(
            "an unfiltered \(verb) covers the whole collection; if you mean it, \(verb)({ all: true })")
    }

    // `require n`: the write is refused, and put back whole, unless it
    // wrote exactly n rows -- a check and its write in one statement.
    private static func requireClause(_ n: Int?) throws -> String {
        guard let n else { return "" }
        if n < 0 { throw FenecError.builder("require takes a count of rows, a whole number from 0 (got \(n))") }
        return " require \(n)"
    }

    private static func docs(_ v: Value) -> [Value] {
        if case .array(let list) = v { return list }
        return [v]
    }

    /// The `put` of a document -- a `Value` or `Row` of fields, a `Codable`
    /// `FenecValue` -- or a list of them, not run.
    /// `ifAbsent`: `put ... if absent`, which passes over a document whose
    /// id or `@unique` value a row holds and counts only what it wrote -- a
    /// lock taken, or not, in one statement. `require`: refused, and nothing
    /// written, unless it wrote exactly that many rows.
    public func toInsert(_ docs: any FenecValue, ifAbsent: Bool = false, require: Int? = nil) throws -> (text: String, params: [Value]) {
        try assertPlain("insert")
        let list = Query.docs(try docs.fenecValue())
        guard !list.isEmpty else { throw FenecError.builder("cannot write an empty document list") }
        let bind = Binder()
        let body = try list.map { try Builder.renderDoc($0, bind, insert: true) }.joined(separator: ", ")
        let absent = ifAbsent ? " if absent" : ""
        let required = try Query.requireClause(require)
        return ("put \(collection) \(list.count == 1 ? body : "[\(body)]")\(absent)\(required)", bind.params)
    }

    /// The `set` of the rows the filter names, not run; with no filter it is
    /// refused unless `all`; with `require`, refused unless it set exactly
    /// that many rows.
    public func toUpdate(_ patch: any FenecValue, all: Bool = false, require: Int? = nil) throws -> (text: String, params: [Value]) {
        try assertPlain("update")
        let bind = Binder()
        let body = try Builder.renderDoc(try patch.fenecValue(), bind)
        let filter = try requireFilter("update", all, bind)
        return ("set \(collection) \(body)\(filter)\(try Query.requireClause(require))", bind.params)
    }

    /// The `del` of the rows the filter names, not run; with no filter it is
    /// refused unless `all`; with `require`, refused unless it deleted
    /// exactly that many rows.
    public func toDelete(all: Bool = false, require: Int? = nil) throws -> (text: String, params: [Value]) {
        try assertPlain("delete")
        let bind = Binder()
        let filter = try requireFilter("delete", all, bind)
        return ("del \(collection)\(filter)\(try Query.requireClause(require))", bind.params)
    }

    // -------------------------------------------------------------- running

    private func run(_ statement: (text: String, params: [Value])) async throws -> Answer {
        guard let exec else {
            throw FenecError.builder(
                "query is not bound to a connection: use db.from(...) (toFenecQL() if you only want the text)")
        }
        return try await exec(statement.text, statement.params)
    }

    /// Runs the query and hands back its rows.
    public func rows() async throws -> [Row] { try await run(toFenecQL()).rows }

    /// Runs the query and hands back its whole answer: the rows, and what
    /// `facet` counted beside them (`.facets`).
    public func answer() async throws -> Answer { try await run(toFenecQL()) }

    /// Runs the query and decodes its rows as `T`, field by field.
    public func rows<T: Decodable>(as type: T.Type) async throws -> [T] { try decoded(try await rows()) }

    /// The first row with `limit 1`, or `nil`.
    public func first() async throws -> Row? { try await limit(1).rows().first }

    /// The first row with `limit 1` as a `T`, or `nil`.
    public func first<T: Decodable>(as type: T.Type) async throws -> T? { try await limit(1).rows(as: T.self).first }

    /// How many rows match: `get ... count`, no row decoded.
    public func count() async throws -> Int {
        let rows = try await with { $0.count = true }.rows()
        return rows.first?["count"]?.int ?? 0
    }

    /// The path the query took, a line a step; the query runs to tell.
    public func explain() async throws -> [String] {
        let (text, params) = try toFenecQL()
        return try await run(("explain \(text)", params)).rows.map { $0["plan"]?.string ?? "" }
    }

    /// Puts a document, or a list of them: how many it wrote, which with
    /// `ifAbsent` leaves out those already held. None is no statement.
    /// `require`: refused (`.unmet`) unless it wrote exactly that many.
    @discardableResult
    public func insert(_ docs: any FenecValue, ifAbsent: Bool = false, require: Int? = nil) async throws -> Int {
        let v = try docs.fenecValue()
        if case .array(let list) = v, list.isEmpty { return 0 }
        return try await run(toInsert(v, ifAbsent: ifAbsent, require: require)).affected
    }

    /// Sets the patch's fields on the rows the filter names; with no filter
    /// it is refused unless `all`; with `require`, refused (`.unmet`) unless
    /// it set exactly that many rows.
    @discardableResult
    public func update(_ patch: any FenecValue, all: Bool = false, require: Int? = nil) async throws -> Int {
        try await run(toUpdate(patch, all: all, require: require)).affected
    }

    /// Deletes the rows the filter names; with no filter it is refused
    /// unless `all`; with `require`, refused (`.unmet`) unless it deleted
    /// exactly that many rows.
    @discardableResult
    public func delete(all: Bool = false, require: Int? = nil) async throws -> Int {
        try await run(toDelete(all: all, require: require)).affected
    }
}
