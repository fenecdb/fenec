import Foundation

/// A fenec-server over HTTP, with no file on the device: every query goes
/// to the server (`POST /query`), through the same builder. What
/// `Fenec.connect` makes, as `connect` does in JS.
///
///     let db = Fenec.connect(url: "https://api.example.com", token: jwt)
///     let open = try await db.from("todos").where("done", false).rows()
///
/// The requests are `URLSession`'s, so TLS is the system's.
public final class FenecRemote: @unchecked Sendable {
    /// Where a connection's reads go. `.home`, the default: every request
    /// to the tenant's home. `.nearest`: the builder's reads, and an
    /// `asRead()` copy's requests, are marked (`Fenec-Read: replica`) so
    /// that a fenec-shard router near a region holding the tenant's read
    /// replica answers them there, each sent after the furthest position an
    /// answer from home named to this connection (`Fenec-After`,
    /// `Fenec-History`: the change and its history) -- never older than its
    /// own writes, nor a write a failover replaced. Writes go home.
    public enum Reads: Sendable { case home, nearest }

    public let url: String
    /// Where this connection's reads go (`Fenec.connect`'s `reads`).
    public let reads: Reads
    /// The token, the last `Fenec-Seq` and the furthest position, shared
    /// with every copy `withIdempotencyKey`, `withWait` and `asRead` make,
    /// as Go's and .NET's copies share theirs.
    private let shared: Shared
    private let session: URLSession
    private let idempotencyKey: String?
    /// How long the server may hold a write that writes nothing (`withWait`).
    private let held: Double?
    /// This copy's requests only read (`asRead`, the builder's reads).
    private let reading: Bool

    private final class Shared: @unchecked Sendable {
        let lock = NSLock()
        var token: String?
        /// The furthest position an answer from home named, which a read
        /// near is sent after: an answer that arrives late is not the last.
        var at: Position?
        init(token: String?) { self.token = token }
    }

    init(url: String, token: String?, reads: Reads = .home, session: URLSession = .shared) {
        self.url = url.hasSuffix("/") ? String(url.dropLast()) : url
        self.reads = reads
        self.shared = Shared(token: token)
        self.session = session
        self.idempotencyKey = nil
        self.held = nil
        self.reading = false
    }

    private init(_ of: FenecRemote, key: String?, held: Double?, reading: Bool) {
        url = of.url
        reads = of.reads
        shared = of.shared
        session = of.session
        idempotencyKey = key
        self.held = held
        self.reading = reading
    }

    /// A fresh token for the requests from here on.
    public func setToken(_ token: String?) {
        shared.lock.lock()
        shared.token = token
        shared.lock.unlock()
    }

    private func currentToken() -> String? {
        shared.lock.lock()
        defer { shared.lock.unlock() }
        return shared.token
    }

    /// The furthest change an answer from home -- a write's, a read-only
    /// batch's -- left the database at (`Fenec-Seq`), through this
    /// connection or a copy of it; nil before any. Answers arriving out of
    /// order do not move it back, a read near's answer, which may be a
    /// replica's, moves it not, and a read near's 504 sets it to where home
    /// says it stands, as Go's `Seq()` and .NET's `Seq` are.
    public var seq: Int? { position()?.seq }

    /// A copy whose writes -- `run`, `batch` and the builder's -- carry
    /// `key` as their `Idempotency-Key`: sent again after a timeout, a write
    /// is answered as it was the first time and not made twice. One key a
    /// write: the same key with another request is refused (status 422).
    ///
    ///     try await db.withIdempotencyKey(orderID).from("orders").insert(order)
    public func withIdempotencyKey(_ key: String) -> FenecRemote {
        FenecRemote(self, key: key, held: held, reading: reading)
    }

    /// A copy whose `set` and `del` statements -- `run`, `batch` and the
    /// builder's `update`, `delete` and their `Returning` forms -- the
    /// server holds while they write nothing, until they can or for
    /// `seconds`, at most 30 (`Fenec-Wait`): a worker's claim waits there
    /// for a job -- one enqueued, a delayed one come due, a lease lapsed --
    /// rather than sleeping and claiming again. Cancelling the task gives
    /// it up.
    ///
    ///     let claimed = try await db.withWait(30).from("jobs").where(.raw("run_at <= now()"))
    ///         .order("run_at").limit(10).updateReturning(claim)
    public func withWait(_ seconds: Double) -> FenecRemote {
        FenecRemote(self, key: idempotencyKey, held: seconds, reading: reading)
    }

    /// A copy whose requests only read: on a connection made with `reads:
    /// .nearest` a router near the tenant's read replica answers them
    /// there, after this connection's writes. The builder's reads go so on
    /// their own; a text the caller knows reads -- a `run`, a `batch` of
    /// `toFenecQL()`s -- goes through this, and one that writes is refused
    /// (status 403), never landed. On a connection reading home it changes
    /// nothing.
    ///
    ///     let n = try await db.asRead().query("get orders where customer = $1 count", "c-7")
    public func asRead() -> FenecRemote {
        FenecRemote(self, key: idempotencyKey, held: held, reading: true)
    }

    /// Runs FenecQL on the server with `params` for `$1`, `$2` ...
    public func run(_ text: String, _ params: any FenecValue...) async throws -> Answer {
        try await run(text, params: try params.map { try $0.fenecValue() })
    }

    /// `run`, its parameters as values. With `idempotencyKey` a write runs
    /// once however often it is sent.
    public func run(_ text: String, params: [Value], idempotencyKey: String? = nil) async throws -> Answer {
        var body = Row()
        body["query"] = .string(text)
        body["params"] = .array(params)
        let (v, _, _) = try await post(
            "/query", type: "application/json", body: Data(Value.object(body).json.utf8),
            key: idempotencyKey)
        return FenecRemote.answer(v)
    }

    /// What `batch` answers: each statement's answer in order, the change
    /// the batch left the database at, and whether the answer is the one
    /// kept for its idempotency key (`replayed`) -- a replayed answer
    /// carries no `seq`.
    public struct BatchAnswer: Sendable, Equatable {
        public let results: [Answer]
        public let seq: Int?
        public let replayed: Bool
    }

    /// `POST /batch`: the statements in order under one write lock, as one
    /// block -- their writes all land, or at the first error none of them
    /// do, a `FenecError` whose `at` is the statement that stopped it
    /// (`unmet` for a write's `require` not met, its `reason` the one the
    /// statement gave, which stays put as statements go in front of it
    /// where `at` moves). A statement is what the builder's `toInsert`,
    /// `toUpdate`, `toDelete` and `toFenecQL` make.
    /// With `idempotencyKey` a retry after a timeout is answered as the
    /// first try was and writes nothing twice; a batch of reads alone takes
    /// no key.
    public func batch(
        _ statements: [(text: String, params: [Value])], idempotencyKey: String? = nil
    ) async throws -> BatchAnswer {
        let lines = statements.map { s -> String in
            var line = Row()
            line["query"] = .string(s.text)
            line["params"] = .array(s.params)
            return Value.object(line).json
        }
        let (v, seq, replayed) = try await post(
            "/batch", type: "application/x-ndjson", body: Data(lines.joined(separator: "\n").utf8),
            key: idempotencyKey)
        let results = (v["results"]?.array ?? []).map(FenecRemote.answer)
        return BatchAnswer(results: results, seq: seq, replayed: replayed)
    }

    /// A POST's JSON answer, its `Fenec-Seq` and whether it was replayed;
    /// a refusal as a `FenecError` with its status and, for a batch, where
    /// it stopped.
    private func post(_ path: String, type: String, body: Data, key: String?) async throws -> (Value, Int?, Bool) {
        guard let u = URL(string: "\(url)\(path)") else {
            throw FenecError(code: .misuse, message: "not a URL: \(url)")
        }
        var req = URLRequest(url: u)
        req.httpMethod = "POST"
        req.setValue(type, forHTTPHeaderField: "content-type")
        if let token = currentToken() { req.setValue("Bearer \(token)", forHTTPHeaderField: "authorization") }
        if let key = key ?? idempotencyKey { req.setValue(key, forHTTPHeaderField: "idempotency-key") }
        if let held, held > 0 {
            req.setValue(String(Int(held * 1000)), forHTTPHeaderField: "fenec-wait")
            // Held that long before the first byte of its answer.
            req.timeoutInterval = req.timeoutInterval + held
        }
        // A read near: marked for the tenant's read replica in the router's
        // region, after the furthest position home named to this
        // connection, which the replica waits for -- that change of that
        // history.
        var near = reading && reads == .nearest
        // A position of change 0 is none: nothing to wait for, as Go and
        // .NET send none.
        let sent = near ? position().flatMap { $0.seq > 0 ? $0 : nil } : nil
        if near {
            req.setValue("replica", forHTTPHeaderField: "fenec-read")
            if let sent {
                req.setValue(String(sent.seq), forHTTPHeaderField: "fenec-after")
                if let h = sent.history { req.setValue(h, forHTTPHeaderField: "fenec-history") }
            }
        }
        req.httpBody = body
        var (data, http) = try await exchange(req)
        // 504: no copy holds that write in the wait -- home had none
        // either, lost in a failover. Home says where it stands, which the
        // connection takes as its position, lower or not (`told`); asked
        // home again with no mark, the read is answered with what home
        // holds.
        if near, let sent, http?.statusCode == 504 {
            if let home = try? Value.parse(bytes: Array(data)), let at = home["seq"]?.int {
                told(sent: sent, Position(seq: at, history: home["history"]?.string))
            }
            for name in ["fenec-read", "fenec-after", "fenec-history"] { req.setValue(nil, forHTTPHeaderField: name) }
            near = false
            (data, http) = try await exchange(req)
        }
        let status = http?.statusCode ?? 0
        let v = data.isEmpty ? Value.null : (try? Value.parse(bytes: Array(data)))
        guard (200..<300).contains(status) else {
            throw FenecRemote.refused(status, v, retryAfter: http?.value(forHTTPHeaderField: "retry-after"))
        }
        guard let v else {
            throw FenecError(code: .io, message: "the server did not answer JSON (\(status))")
        }
        let seq = (http?.value(forHTTPHeaderField: "fenec-seq")).flatMap { Int($0) }
        // A position from home alone: a read near's answer may be a
        // replica's, behind home or on a branch a failover left.
        if let seq, !near { note(Position(seq: seq, history: http?.value(forHTTPHeaderField: "fenec-history"))) }
        return (v, seq, http?.value(forHTTPHeaderField: "idempotent-replayed") == "true")
    }

    /// The request sent: its answer's body and head.
    private func exchange(_ req: URLRequest) async throws -> (Data, HTTPURLResponse?) {
        do {
            let (data, response) = try await session.data(for: req)
            return (data, response as? HTTPURLResponse)
        } catch {
            throw FenecError(code: .io, message: "the server could not be reached: \(error.localizedDescription)")
        }
    }

    // Out of the async function: an NSLock is not to be taken in one.
    private func note(_ p: Position) {
        shared.lock.lock()
        if p.isPast(shared.at) { shared.at = p }
        shared.lock.unlock()
    }

    private func position() -> Position? {
        shared.lock.lock()
        defer { shared.lock.unlock() }
        return shared.at
    }

    /// Home's position, lower or not, which a read near's 504 names when no
    /// copy held the one it was sent after -- a write lost in a failover:
    /// kept, every read near would wait it out for good. Taken only over
    /// the position sent -- a write answered meanwhile is further, and set
    /// back the next read near would miss it -- and never of an earlier
    /// term of the same lineage: a router whose copy of the directory lags
    /// a failover sends the read to the old primary, fenced, which names
    /// the branch it abandoned. Another lineage's is a tenant made again
    /// under the name, which starts again at term 1.
    private func told(sent: Position, _ p: Position) {
        shared.lock.lock()
        if shared.at == sent && (p.term >= sent.term || p.isOfAnotherLineage(than: sent)) { shared.at = p }
        shared.lock.unlock()
    }

    /// The rows a statement answers.
    public func query(_ text: String, _ params: any FenecValue...) async throws -> [Row] {
        try await run(text, params: try params.map { try $0.fenecValue() }).rows
    }

    /// A write: how many documents it wrote.
    @discardableResult
    public func execute(_ text: String, _ params: any FenecValue...) async throws -> Int {
        try await run(text, params: try params.map { try $0.fenecValue() }).affected
    }

    /// The query builder, its queries run on the server -- its reads as an
    /// `asRead()` copy's, near on a connection reading near.
    public func from(_ collection: String) throws -> Query {
        let reader = asRead()
        return try Query.from(collection).bind(
            { [self] text, params in try await self.run(text, params: params) },
            read: { text, params in try await reader.run(text, params: params) })
    }

    /// A refusal as the error it is: its status and message, where a batch
    /// stopped, what a `require n else '<reason>'` that refused it said,
    /// and where a tenant's quota refused it the quota and how
    /// many seconds to wait -- to the millisecond where the body says it
    /// (`retry_after_ms`), else `Retry-After`, or the same in the body.
    static func refused(_ status: Int, _ v: Value?, retryAfter: String?) -> FenecError {
        let quota = v?["quota"]?.string
        let ms = v?["retry_after_ms"]?.int.flatMap { $0 > 0 ? Double($0) / 1000 : nil }
        let wait = ms ?? retryAfter.flatMap { Double($0) } ?? v?["retry_after"]?.int.map(Double.init)
        return FenecError(
            code: quota == nil ? FenecRemote.code(status) : .quota,
            message: v?["error"]?.string ?? "HTTP \(status)",
            status: status, at: v?["at"]?.int, completed: v?["completed"]?.int,
            quota: quota, retryAfter: wait, reason: v?["reason"]?.string)
    }

    /// An HTTP status as the error kind the engine would have said; the
    /// status itself is the error's `status`.
    static func code(_ status: Int) -> FenecError.Code {
        switch status {
        case 400: return .query
        case 401, 403: return .denied
        case 404: return .notFound
        case 409: return .duplicate
        case 412: return .unmet
        default: return .io
        }
    }

    /// The endpoint's answer as the library's: rows come as an array, or --
    /// when the query asked for facets, and as a batch's each read -- as
    /// `{"rows": [...], "facets": {...}}`, the counts beside the rows
    /// rather than in one.
    static func answer(_ v: Value) -> Answer {
        if let rows = v.array ?? v["rows"]?.array {
            let objects = rows.compactMap(\.object)
            return .rows(columns: objects.first?.keys ?? [], rows: objects, facets: Facets(json: v["facets"]))
        }
        if let n = v["affected"]?.int { return .affected(n) }
        if let c = v["collections"]?.array { return .schemas(c.compactMap(\.object)) }
        if let m = v["message"]?.string ?? v["ok"]?.string { return .ok(m) }
        return .ok(v.json)
    }
}

/// Where an answer from home left the tenant: its change (`Fenec-Seq`) and
/// the history that change is of (`Fenec-History`, `<term>-<id>`; none for a
/// database that never had a replica).
struct Position: Equatable, Sendable {
    let seq: Int
    let history: String?

    /// The history's term, one more at each promotion.
    var term: Int {
        guard let h = history, let t = h.split(separator: "-", maxSplits: 1).first else { return 0 }
        return Int(t) ?? 0
    }

    /// The history's lineage: its id's top half, which every fork of the
    /// tenant keeps and a tenant made again under its name does not.
    var lineage: Substring? {
        guard let h = history, let dash = h.firstIndex(of: "-") else { return nil }
        let id = h[h.index(after: dash)...]
        return id.count >= 8 ? id.prefix(8) : nil
    }

    /// Whether this is of another lineage than `q`: a tenant made again.
    func isOfAnotherLineage(than q: Position) -> Bool {
        guard let a = lineage, let b = q.lineage else { return false }
        return a != b
    }

    /// Whether this is past `q`: of another lineage -- home's word on a
    /// tenant made again, whose terms start again -- or a later term of the
    /// same, or further in one.
    func isPast(_ q: Position?) -> Bool {
        guard let q else { return true }
        return isOfAnotherLineage(than: q) || term > q.term || (term == q.term && seq > q.seq)
    }
}

extension Fenec {
    /// A fenec-server over HTTP, with no local file: every query goes to the
    /// server, through the same builder (`FenecRemote`). With `reads:
    /// .nearest` the builder's reads go to the tenant's read replica near
    /// the app, where a fenec-shard router has one, after the app's own
    /// writes (`FenecRemote.Reads`).
    public static func connect(url: String, token: String? = nil, reads: FenecRemote.Reads = .home) -> FenecRemote {
        FenecRemote(url: url, token: token, reads: reads)
    }
}
