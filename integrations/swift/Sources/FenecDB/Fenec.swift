import FenecFFI
import Foundation

/// What the library or the builder refused, and why. `code` is the kind:
/// the engine's (`notFound`, `duplicate` ...), the boundary's (`misuse`,
/// `locked`, `panic`), or `builder` for a chain the query builder refused
/// before anything ran -- its message the JS builder's, word for word.
public struct FenecError: Error, CustomStringConvertible, Sendable, Equatable {
    public enum Code: Int32, Sendable {
        case type = 1, notFound, exists, duplicate, corrupt, query, io, plugin, readOnly, denied
        case panic, misuse, locked
        case builder = 100
    }

    public let code: Code
    public let message: String
    /// The parameters the library asked for again as JSON (`exact`).
    let exact: [Int]?

    public init(code: Code, message: String) {
        self.init(code: code, message: message, exact: nil)
    }

    init(code: Code, message: String, exact: [Int]?) {
        self.code = code
        self.message = message
        self.exact = exact
    }

    public var description: String { message }

    static func builder(_ message: String) -> FenecError { FenecError(code: .builder, message: message) }
}

/// An answer to a statement: rows, a count of what was written, a word
/// that it was done, or the schemas `collections` and `describe` give.
/// Rows carry what `facet` counted beside them -- never in a row, since the
/// counts are over every row the query matched, not the page.
public enum Answer: Sendable, Equatable {
    case rows(columns: [String], rows: [Row], facets: Facets = Facets())
    case affected(Int)
    case ok(String)
    case schemas([Row])

    /// The rows, or none.
    public var rows: [Row] {
        if case .rows(_, let r, _) = self { return r }
        return []
    }

    /// What the query's `facet` clauses counted; empty when it asked none.
    public var facets: Facets {
        if case .rows(_, _, let f) = self { return f }
        return Facets()
    }

    /// How many documents a write wrote, or 0.
    public var affected: Int {
        if case .affected(let n) = self { return n }
        return 0
    }
}

/// One value a `facet` counted and how many matched rows hold it: any value
/// the field holds, `.null` for the rows where it is null.
public struct FacetCount: Sendable, Equatable {
    public let value: Value
    public let count: Int

    public init(value: Value, count: Int) {
        self.value = value
        self.count = count
    }
}

/// Each `facet` field's counts, in the order the query asked them, a
/// field's values most first. Not a dictionary, which would lose that order.
public struct Facets: Sendable, Equatable, Sequence {
    public private(set) var fields: [String] = []
    private var counts: [[FacetCount]] = []

    public init() {}

    public var isEmpty: Bool { fields.isEmpty }

    /// A field's counts, by the name -- or path -- `facet` was given.
    public subscript(field: String) -> [FacetCount]? {
        fields.firstIndex(of: field).map { counts[$0] }
    }

    public func makeIterator() -> AnyIterator<(field: String, counts: [FacetCount])> {
        var i = 0
        return AnyIterator {
            guard i < fields.count else { return nil }
            defer { i += 1 }
            return (fields[i], counts[i])
        }
    }

    /// `{"brand": [{"value": "acme", "count": 12}, ...], ...}`, as the server
    /// and the library both answer them; none where it is absent.
    init(json: Value?) {
        guard let object = json?.object else { return }
        for (field, list) in object {
            fields.append(field)
            counts.append((list.array ?? []).map {
                FacetCount(value: $0["value"] ?? .null, count: $0["count"]?.int ?? 0)
            })
        }
    }
}

/// A fenecdb database in a file on the device -- or in memory -- through
/// the native library (`crates/fenec-ffi`): the engine and the file format
/// of the server and the browser.
///
///     let db = try await Fenec.open(path: docs.appendingPathComponent("app.fenec").path)
///     try await db.execute("create collection todos (title text, done bool @hash)")
///     try await db.from("todos").insert(["title": "milk", "done": false] as Value)
///     let open = try await db.from("todos").where("done", false).rows()
///
/// Every call runs off the caller's thread, on a queue of the library's: a
/// read waits for a write under way, a write for its fsync. One instance is
/// safe to share between tasks and threads; open a file once in a process --
/// a second open of it is refused (`locked`), since two databases over one
/// file corrupt it.
public final class Fenec: @unchecked Sendable {
    /// How `open` keeps the file.
    public struct Options: OptionSet, Sendable {
        public let rawValue: UInt32
        public init(rawValue: UInt32) { self.rawValue = rawValue }
        /// Writes wait in a buffer for `sync()`, `flush()` or `close()`,
        /// rather than an fsync each before the call returns -- a few
        /// milliseconds a write on an iPhone or a Mac.
        public static let noSync = Options(rawValue: UInt32(FENEC_OPEN_NO_SYNC))
        /// The file read into memory rather than mapped: for a file under
        /// iOS's `complete` protection class, whose pages become unreadable
        /// as the device locks -- a mapped page read then is the app's end.
        public static let inMemory = Options(rawValue: UInt32(FENEC_OPEN_IN_MEMORY))
    }

    let handle: UInt64
    private let state = NSLock()
    private var closed = false
    /// Writes under way, which a live query's run waits out.
    private var inflight = 0
    let lives: Lives
    /// The replica's sync, set once by `Fenec.sync`.
    private var replicaSync: Replica?

    private init(handle: UInt64) {
        self.handle = handle
        self.lives = Lives()
    }

    // Let go of without `close()`, the database closes as it is freed, on
    // whichever thread let it go: its graphs saved and its file synced, which
    // may take the main thread milliseconds. An app closes it itself.
    deinit {
        if !closed {
            // The session holds its delegate, the replica, until it is let
            // go of: left running, its streams outlived the database.
            replicaSync?.stopNow()
            var out: UnsafeMutablePointer<CChar>?
            _ = fenec_close(handle, &out, nil)
            fenec_free_string(out)
        }
    }

    /// The library's version.
    public static var version: String { String(cString: fenec_version()) }

    /// Opens the file at `path`, made when missing. The open reads the
    /// file's index and links the vectors written since its graphs were last
    /// saved, off the caller's thread.
    public static func open(path: String, options: Options = []) async throws -> Fenec {
        try await background {
            var handle: UInt64 = 0
            try call { out, len in
                var p = path
                return p.withUTF8 { fenec_open($0.baseAddress, $0.count, options.rawValue, &handle, out, len) }
            }
            return Fenec(handle: handle)
        }
    }

    /// Opens the file at `url`.
    public static func open(_ url: URL, options: Options = []) async throws -> Fenec {
        try await open(path: url.path, options: options)
    }

    /// A database in memory alone.
    public static func memory() throws -> Fenec {
        var handle: UInt64 = 0
        try call { out, len in fenec_open_memory(&handle, out, len) }
        return Fenec(handle: handle)
    }

    // -------------------------------------------------------------- running

    /// Runs FenecQL -- one statement or several, which land together -- with
    /// `params` for `$1`, `$2` ... and answers.
    public func run(_ text: String, _ params: any FenecValue...) async throws -> Answer {
        try await run(text, params: try params.map { try $0.fenecValue() })
    }

    /// `run`, its parameters as values.
    public func run(_ text: String, params: [Value]) async throws -> Answer {
        try await run(text, params: params, quiet: false)
    }

    /// The rows a statement answers.
    public func query(_ text: String, _ params: any FenecValue...) async throws -> [Row] {
        try await run(text, params: try params.map { try $0.fenecValue() }).rows
    }

    /// The rows a statement answers, each decoded as `T` by its fields.
    public func query<T: Decodable>(as type: T.Type, _ text: String, _ params: any FenecValue...) async throws -> [T] {
        try decoded(try await run(text, params: try params.map { try $0.fenecValue() }).rows)
    }

    /// A write: how many documents it wrote.
    @discardableResult
    public func execute(_ text: String, _ params: any FenecValue...) async throws -> Int {
        try await run(text, params: try params.map { try $0.fenecValue() }).affected
    }

    /// The query builder over a collection: `db.from("todos").where("done", false).rows()`.
    public func from(_ collection: String) throws -> Query {
        try Query.from(collection).bind { [weak self] text, params in
            guard let self else { throw FenecError(code: .misuse, message: "the database was let go of") }
            return try await self.run(text, params: params)
        }
    }

    func run(_ text: String, params: [Value], quiet: Bool) async throws -> Answer {
        if isClosed { throw FenecError(code: .misuse, message: "the database was closed") }
        if !quiet { begin() }
        defer { if !quiet { end() } }
        let handle = self.handle
        return try await Fenec.background {
            do {
                return try Fenec.answer(handle: handle, text: text, params: params, asJson: [])
            } catch let e as FenecError where e.exact != nil {
                // A json field keeps a list of numbers as written: those
                // the library asks for go again inside the JSON.
                return try Fenec.answer(handle: handle, text: text, params: params, asJson: Set(e.exact!))
            }
        }
    }

    private func begin() {
        state.lock()
        inflight += 1
        state.unlock()
    }

    private func end() {
        state.lock()
        inflight -= 1
        state.unlock()
        // After an error too: a text that failed may follow statements that
        // wrote, and the change ring says what landed.
        lives.touch(self)
        // A write to a synced collection left a request for the server due.
        syncing?.poll()
    }

    var syncing: Replica? {
        state.lock()
        defer { state.unlock() }
        return replicaSync
    }

    func attach(_ r: Replica) {
        state.lock()
        replicaSync = r
        state.unlock()
    }

    var isClosed: Bool {
        state.lock()
        defer { state.unlock() }
        return closed
    }

    var writing: Bool {
        state.lock()
        defer { state.unlock() }
        return inflight > 0
    }

    // ------------------------------------------------------------- changes

    /// What changed since `since`: the counter now, and the collections
    /// written -- `nil` when it cannot be told which, everything stale.
    public func changes(since: UInt64) async throws -> Changes {
        let handle = self.handle
        return try await Fenec.background { try Fenec.changes(handle: handle, since: since) }
    }

    static func changes(handle: UInt64, since: UInt64) throws -> Changes {
        let out = try call { out, len in fenec_changes(handle, since, out, len) }
        let v = try Value.parse(bytes: out ?? [])
        return Changes(
            seq: UInt64(v["seq"]?.int ?? 0),
            horizon: UInt64(v["horizon"]?.int ?? 0),
            collections: v["collections"]?.array?.compactMap(\.string)
        )
    }

    // ----------------------------------------------------------- durability

    /// Every write so far on disk: written and fsynced, the fsync with no
    /// lock held. What a database opened `.noSync` calls when its writes
    /// must last.
    public func sync() async throws { try await byHandle(.sync) }

    /// Every write so far handed to the system, no fsync: it outlives the
    /// app being killed, not the device losing power -- microseconds, what
    /// an app does as it goes to the background.
    public func flush() async throws { try await byHandle(.flush) }

    /// The file written anew as an image of the database, graphs and all:
    /// the next open links nothing. Holds the database while it writes.
    public func checkpoint() async throws { try await byHandle(.checkpoint) }

    /// Saves the graphs, syncs and lets the file go; the live queries stop.
    /// Waits for the calls under way.
    public func close() async throws {
        if markClosed() { return }
        await syncing?.stop()
        await lives.clear()
        try await byHandle(.close)
    }

    /// Whether it was closed before.
    private func markClosed() -> Bool {
        state.lock()
        defer { state.unlock() }
        let was = closed
        closed = true
        return was
    }

    private enum Op { case sync, flush, checkpoint, close }

    private func byHandle(_ op: Op) async throws {
        let handle = self.handle
        try await Fenec.background {
            try Fenec.call { out, len in
                switch op {
                case .sync: return fenec_sync(handle, out, len)
                case .flush: return fenec_flush(handle, out, len)
                case .checkpoint: return fenec_checkpoint(handle, out, len)
                case .close: return fenec_close(handle, out, len)
                }
            }
        }
    }

    // -------------------------------------------------------- the boundary

    /// The queue the library's calls run on: each may block -- for the
    /// lock, an fsync -- which is what a cooperative thread of Swift's own
    /// pool should not do.
    static let queue = DispatchQueue(label: "dev.fenecdb", qos: .userInitiated, attributes: .concurrent)

    @discardableResult
    static func background<T: Sendable>(_ body: @escaping @Sendable () throws -> T) async throws -> T {
        try await withCheckedThrowingContinuation { cont in
            queue.async { cont.resume(with: Result { try body() }) }
        }
    }

    /// One call: its code, and what it wrote through its out pointer -- the
    /// answer's bytes, or the error it is thrown as.
    @discardableResult
    static func call(
        _ f: (UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>, UnsafeMutablePointer<Int>) -> Int32
    ) throws -> [UInt8]? {
        var out: UnsafeMutablePointer<CChar>?
        var len = 0
        let code = f(&out, &len)
        defer { fenec_free_string(out) }
        let bytes = out.map { p in p.withMemoryRebound(to: UInt8.self, capacity: len) { Array(UnsafeBufferPointer(start: $0, count: len)) } }
        guard code != FENEC_OK else { return bytes }
        let v = (try? Value.parse(bytes: bytes ?? [])) ?? .null
        throw FenecError(
            code: FenecError.Code(rawValue: code) ?? .panic,
            message: v["message"]?.string ?? "error \(code)",
            exact: v["exact"]?.array?.compactMap(\.int)
        )
    }

    /// Runs `text`: the vectors among the parameters -- a `[Float]`, or a
    /// list of finite numbers, which the engine reads as a vector either way
    /// -- go over as their bytes, their place `null` in the JSON, unless
    /// `asJson` names them. Written out as text and read back, a vector's
    /// digits were most of a put's time: 4.6 us a 128-dim put as bytes,
    /// 23.3 as JSON (`make ffi-bench`).
    static func answer(handle: UInt64, text: String, params: [Value], asJson: Set<Int>) throws -> Answer {
        var json = params
        var vectors: [UInt8] = []
        for (i, p) in params.enumerated() where !asJson.contains(i) {
            guard let f = vector(p) else { continue }
            json[i] = .null
            append(UInt32(i), &vectors)
            append(UInt32(f.count), &vectors)
            for x in f {
                // A -0 goes as 0, as JSON writes it: either way, one vector.
                append((x == 0 ? 0 : x).bitPattern, &vectors)
            }
        }
        var t = text
        var p = Value.array(json).json
        let out = try t.withUTF8 { tb in
            try p.withUTF8 { pb in
                try vectors.withUnsafeBufferPointer { vb in
                    try call { out, len in
                        fenec_query(handle, tb.baseAddress, tb.count, pb.baseAddress, pb.count, vb.baseAddress, vb.count, out, len)
                    }
                }
            }
        }
        return try Fenec.decode(answer: try Value.parse(bytes: out ?? []))
    }

    private static func vector(_ v: Value) -> [Float]? {
        switch v {
        case .floats(let f) where !f.isEmpty: return f
        case .array(let a) where !a.isEmpty:
            var out: [Float] = []
            out.reserveCapacity(a.count)
            for x in a {
                switch x {
                case .int(let n): out.append(Float(n))
                case .double(let d) where d.isFinite: out.append(Float(d))
                default: return nil
                }
            }
            return out
        default: return nil
        }
    }

    private static func append(_ n: UInt32, _ out: inout [UInt8]) {
        withUnsafeBytes(of: n.littleEndian) { out.append(contentsOf: $0) }
    }

    static func decode(answer v: Value) throws -> Answer {
        switch v["kind"]?.string {
        case "rows":
            let r = v["result"]
            return .rows(
                columns: r?["columns"]?.array?.compactMap(\.string) ?? [],
                rows: r?["rows"]?.array?.compactMap(\.object) ?? [],
                facets: Facets(json: r?["facets"])
            )
        case "affected": return .affected(v["count"]?.int ?? 0)
        case "ok": return .ok(v["message"]?.string ?? "")
        case "schemas": return .schemas(v["collections"]?.array?.compactMap(\.object) ?? [])
        default: throw FenecError(code: .query, message: "an answer of no kind the binding knows: \(v.json)")
        }
    }
}

/// What `changes(since:)` answers.
public struct Changes: Sendable, Equatable {
    /// The change counter now: the cursor to ask from next.
    public let seq: UInt64
    /// The oldest cursor the ring still answers for.
    public let horizon: UInt64
    /// The collections written since, or `nil`: everything is stale.
    public let collections: [String]?
}

func decoded<T: Decodable>(_ rows: [Row]) throws -> [T] {
    try JSONDecoder().decode([T].self, from: Data(Value.array(rows.map { .object($0) }).json.utf8))
}
