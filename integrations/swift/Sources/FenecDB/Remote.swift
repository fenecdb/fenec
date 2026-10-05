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
    public let url: String
    /// The token and the last `Fenec-Seq`, shared with every copy
    /// `withIdempotencyKey` makes, as Go's and .NET's copies share theirs.
    private let shared: Shared
    private let session: URLSession
    private let idempotencyKey: String?

    private final class Shared: @unchecked Sendable {
        let lock = NSLock()
        var token: String?
        var seq: Int?
        init(token: String?) { self.token = token }
    }

    init(url: String, token: String?, session: URLSession = .shared) {
        self.url = url.hasSuffix("/") ? String(url.dropLast()) : url
        self.shared = Shared(token: token)
        self.session = session
        self.idempotencyKey = nil
    }

    private init(_ of: FenecRemote, key: String) {
        url = of.url
        shared = of.shared
        session = of.session
        idempotencyKey = key
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

    /// The change the last write through this connection, or a copy of it,
    /// left the database at (`Fenec-Seq`); nil before any.
    public var seq: Int? {
        shared.lock.lock()
        defer { shared.lock.unlock() }
        return shared.seq
    }

    /// A copy whose writes -- `run`, `batch` and the builder's -- carry
    /// `key` as their `Idempotency-Key`: sent again after a timeout, a write
    /// is answered as it was the first time and not made twice. One key a
    /// write: the same key with another request is refused (status 422).
    ///
    ///     try await db.withIdempotencyKey(orderID).from("orders").insert(order)
    public func withIdempotencyKey(_ key: String) -> FenecRemote {
        FenecRemote(self, key: key)
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
    /// (`unmet` for a write's `require` not met). A statement is what the
    /// builder's `toInsert`, `toUpdate`, `toDelete` and `toFenecQL` make.
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
        req.httpBody = body
        let data: Data
        let response: URLResponse
        do {
            (data, response) = try await session.data(for: req)
        } catch {
            throw FenecError(code: .io, message: "the server could not be reached: \(error.localizedDescription)")
        }
        let http = response as? HTTPURLResponse
        let status = http?.statusCode ?? 0
        let v = data.isEmpty ? Value.null : (try? Value.parse(bytes: Array(data)))
        guard (200..<300).contains(status) else {
            throw FenecError(
                code: FenecRemote.code(status), message: v?["error"]?.string ?? "HTTP \(status)",
                status: status, at: v?["at"]?.int, completed: v?["completed"]?.int)
        }
        guard let v else {
            throw FenecError(code: .io, message: "the server did not answer JSON (\(status))")
        }
        let seq = (http?.value(forHTTPHeaderField: "fenec-seq")).flatMap { Int($0) }
        if let seq { note(seq: seq) }
        return (v, seq, http?.value(forHTTPHeaderField: "idempotent-replayed") == "true")
    }

    // Out of the async function: an NSLock is not to be taken in one.
    private func note(seq: Int) {
        shared.lock.lock()
        shared.seq = seq
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

    /// The query builder, its queries run on the server.
    public func from(_ collection: String) throws -> Query {
        try Query.from(collection).bind { [self] text, params in try await self.run(text, params: params) }
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

extension Fenec {
    /// A fenec-server over HTTP, with no local file: every query goes to the
    /// server, through the same builder (`FenecRemote`).
    public static func connect(url: String, token: String? = nil) -> FenecRemote {
        FenecRemote(url: url, token: token)
    }
}
