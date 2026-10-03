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
    private let lock = NSLock()
    private var token: String?
    private let session: URLSession

    init(url: String, token: String?, session: URLSession = .shared) {
        self.url = url.hasSuffix("/") ? String(url.dropLast()) : url
        self.token = token
        self.session = session
    }

    /// A fresh token for the requests from here on.
    public func setToken(_ token: String?) {
        lock.lock()
        self.token = token
        lock.unlock()
    }

    private func currentToken() -> String? {
        lock.lock()
        defer { lock.unlock() }
        return token
    }

    /// Runs FenecQL on the server with `params` for `$1`, `$2` ...
    public func run(_ text: String, _ params: any FenecValue...) async throws -> Answer {
        try await run(text, params: try params.map { try $0.fenecValue() })
    }

    /// `run`, its parameters as values.
    public func run(_ text: String, params: [Value]) async throws -> Answer {
        guard let u = URL(string: "\(url)/query") else {
            throw FenecError(code: .misuse, message: "not a URL: \(url)")
        }
        var req = URLRequest(url: u)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "content-type")
        if let token = currentToken() { req.setValue("Bearer \(token)", forHTTPHeaderField: "authorization") }
        var body = Row()
        body["query"] = .string(text)
        body["params"] = .array(params)
        req.httpBody = Data(Value.object(body).json.utf8)
        let data: Data
        let response: URLResponse
        do {
            (data, response) = try await session.data(for: req)
        } catch {
            throw FenecError(code: .io, message: "the server could not be reached: \(error.localizedDescription)")
        }
        let status = (response as? HTTPURLResponse)?.statusCode ?? 0
        let v = data.isEmpty ? Value.null : (try? Value.parse(bytes: Array(data)))
        guard (200..<300).contains(status) else {
            throw FenecError(
                code: FenecRemote.code(status), message: v?["error"]?.string ?? "HTTP \(status)")
        }
        guard let v else {
            throw FenecError(code: .io, message: "the server did not answer JSON (\(status))")
        }
        return FenecRemote.answer(v)
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

    /// An HTTP status as the error kind the engine would have said.
    static func code(_ status: Int) -> FenecError.Code {
        switch status {
        case 400: return .query
        case 401, 403: return .denied
        case 404: return .notFound
        case 409: return .duplicate
        default: return .io
        }
    }

    /// The endpoint's answer as the library's: rows come as an array, or --
    /// when the query asked for facets -- as `{"rows": [...], "facets":
    /// {...}}`, the counts beside the rows rather than in one.
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
