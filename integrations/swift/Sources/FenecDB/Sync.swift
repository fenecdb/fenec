import FenecFFI
import Foundation

/// The rows of one collection a replica keeps: a filter the server applies,
/// in the JS sync layer's form -- `["status": "open", "priority": ["gte": 3]]`
/// -- the fields it carries, and the business key an optimistic insert is
/// matched with the server's copy by (a `text @hash` field).
public struct Shape: Sendable {
    public let collection: String
    public let condition: Value?
    public let select: [String]?
    public let key: String?

    public init(_ collection: String, where condition: Value? = nil, select: [String]? = nil, key: String? = nil) {
        self.collection = collection
        self.condition = condition
        self.select = select
        self.key = key
    }

    var value: Value {
        var r = Row()
        r["collection"] = .string(collection)
        if let condition { r["where"] = condition }
        if let select { r["select"] = .array(select.map { .string($0) }) }
        if let key { r["key"] = .string(key) }
        return .object(r)
    }
}

/// Where a replica stands with its server.
public struct SyncStatus: Sendable, Equatable {
    public enum State: String, Sendable {
        /// Every shape's stream is open and seeded.
        case online
        /// The server cannot be reached, or the app said the network is gone.
        case offline
        /// Connecting, seeding, or sending the writes left from before.
        case catchingUp = "catching_up"
    }

    public struct Failure: Sendable, Equatable {
        public let message: String
        /// The HTTP status, or 0 for none (the network, or the replica).
        public let status: Int
    }

    public struct ShapeState: Sendable, Equatable {
        public let collection: String
        public let cursor: UInt64
        public let seeded: Bool
        public let connected: Bool
    }

    public let state: State
    /// Writes the server has not answered.
    public let pending: Int
    /// The last error: a connection's until it is back, a refused write's
    /// until the next.
    public let error: Failure?
    public let shapes: [ShapeState]

    /// Whether every shape has had its seed.
    public var seeded: Bool { shapes.allSatisfy(\.seeded) }

    static let starting = SyncStatus(state: .catchingUp, pending: 0, error: nil, shapes: [])

    init(state: State, pending: Int, error: Failure?, shapes: [ShapeState]) {
        self.state = state
        self.pending = pending
        self.error = error
        self.shapes = shapes
    }

    init(_ v: Value) {
        state = State(rawValue: v["state"]?.string ?? "") ?? .catchingUp
        pending = v["pending"]?.int ?? 0
        error = v["error"]?.object.map { Failure(message: $0["message"]?.string ?? "", status: $0["status"]?.int ?? 0) }
        shapes = (v["shapes"]?.array ?? []).map {
            ShapeState(
                collection: $0["collection"]?.string ?? "",
                cursor: UInt64($0["cursor"]?.int ?? 0),
                seeded: $0["seeded"]?.bool ?? false,
                connected: $0["connected"]?.bool ?? false)
        }
    }
}

/// A write the server refused: it was put back on the replica.
public struct Refusal: Sendable, Equatable {
    public let status: Int
    public let message: String
    /// The statement's text, as it was run.
    public let query: String
}

extension Fenec {
    /// A replica of `shapes` in the file at `path`, kept in step with the
    /// server at `url`: reads and live queries are the file's, a write to a
    /// shape's collection is applied at once and sent to the server --
    /// queued in the file while it cannot be reached, and sent under an
    /// idempotency key, so it lands once. The same `Fenec`: `from`, `live`
    /// and `query` work as they do over a file of the app's own.
    ///
    ///     let db = try await Fenec.sync(
    ///         url: "https://api.example.com", token: jwt,
    ///         shapes: [Shape("todos", where: ["done": false], key: "key")],
    ///         path: docs.appendingPathComponent("todos.fenec").path)
    ///
    /// The requests and the change stream are made by `URLSession`, with the
    /// system's TLS: iOS's App Transport Security refuses plain `http://`
    /// to anything but a local address, and a server is reached through
    /// TLS in front of it. `tokenProvider` is asked for a fresh token when
    /// the server answers 401.
    public static func sync(
        url: String, token: String? = nil, shapes: [Shape], path: String, options: Options = [],
        tokenProvider: (@Sendable () async throws -> String)? = nil
    ) async throws -> Fenec {
        let db = try await open(path: path, options: options)
        do {
            let replica = Replica(db: db, url: url, token: token, provider: tokenProvider)
            var r = Row()
            r["url"] = .string(url)
            if let token { r["token"] = .string(token) }
            var seed = SystemRandomNumberGenerator()
            r["seed"] = .string(String(format: "%016llx%016llx", seed.next() as UInt64, seed.next() as UInt64))
            r["shapes"] = .array(shapes.map(\.value))
            let config = Value.object(r).json
            let handle = db.handle
            let first = try await background {
                try call { out, len in
                    var c = config
                    return c.withUTF8 { fenec_sync_start(handle, $0.baseAddress, $0.count, out, len) }
                } ?? []
            }
            db.attach(replica)
            replica.start(first)
            return db
        } catch {
            try? await db.close()
            throw error
        }
    }

    /// A replica's sync, when the database is one (`Fenec.sync`).
    public var replica: Replica? { syncing }
}

/// A replica's sync with its server: its status, its token, and the
/// network as the app sees it. Its work is on a serial queue of its own,
/// where `URLSession` hands it what comes, so the engine is told of a
/// stream's bytes in their order.
public final class Replica: NSObject, URLSessionDataDelegate, @unchecked Sendable {
    private weak var db: Fenec?
    private let handle: UInt64
    private let url: String
    private let provider: (@Sendable () async throws -> String)?
    private let queue = DispatchQueue(label: "dev.fenecdb.sync")
    private var session: URLSession!
    private let lock = NSLock()
    private var token: String?
    private var current = SyncStatus.starting
    private var tasks: [UInt64: URLSessionTask] = [:]
    private var streams: [Int: (id: UInt64, status: Int, body: Data)] = [:]
    private var watchers: [UUID: AsyncStream<SyncStatus>.Continuation] = [:]
    private var refusalWatchers: [UUID: AsyncStream<Refusal>.Continuation] = [:]
    private var stopped = false

    init(db: Fenec, url: String, token: String?, provider: (@Sendable () async throws -> String)?) {
        self.db = db
        self.handle = db.handle
        self.url = url.hasSuffix("/") ? String(url.dropLast()) : url
        self.token = token
        self.provider = provider
        super.init()
        let ops = OperationQueue()
        ops.underlyingQueue = queue
        ops.maxConcurrentOperationCount = 1
        let config = URLSessionConfiguration.default
        // A stream is quiet between changes but for the server's keepalive
        // every 20 s: the idle limit is past it.
        config.timeoutIntervalForRequest = 90
        config.waitsForConnectivity = false
        session = URLSession(configuration: config, delegate: self, delegateQueue: ops)
    }

    // ------------------------------------------------------------ the app's

    /// The status now.
    public var status: SyncStatus {
        lock.lock()
        defer { lock.unlock() }
        return current
    }

    /// The status now and at every change.
    public func statuses() -> AsyncStream<SyncStatus> {
        AsyncStream { cont in
            let id = UUID()
            lock.lock()
            watchers[id] = cont
            let now = current
            lock.unlock()
            cont.yield(now)
            cont.onTermination = { [weak self] _ in
                guard let self else { return }
                self.lock.lock()
                self.watchers[id] = nil
                self.lock.unlock()
            }
        }
    }

    /// The writes the server refused, each put back on the replica.
    public func refusals() -> AsyncStream<Refusal> {
        AsyncStream { cont in
            let id = UUID()
            lock.lock()
            refusalWatchers[id] = cont
            lock.unlock()
            cont.onTermination = { [weak self] _ in
                guard let self else { return }
                self.lock.lock()
                self.refusalWatchers[id] = nil
                self.lock.unlock()
            }
        }
    }

    /// Returns once every shape has had its seed: at once for a replica
    /// opened again, which resumes where it stopped.
    public func ready() async {
        _ = await refresh()
        for await s in statuses() where s.seeded && !s.shapes.isEmpty { return }
    }

    /// Returns once the server has answered every write made so far.
    public func pushed() async {
        _ = await refresh()
        for await s in statuses() where s.pending == 0 { return }
    }

    /// The status once everything told the sync before this call is in
    /// it -- a write just made among its pending ones. `status` is what the
    /// sync last said, and never waits.
    public func refresh() async -> SyncStatus {
        await withCheckedContinuation { cont in
            queue.async {
                self.reload()
                cont.resume(returning: self.status)
            }
        }
    }

    /// A fresh token, for the requests and streams from here on.
    public func setToken(_ token: String) {
        lock.lock()
        self.token = token
        lock.unlock()
        signal(["token": .string(token)])
    }

    /// The network as the platform sees it (`NWPathMonitor`): offline closes
    /// the streams and sends nothing; online comes back at once, the
    /// backoff forgotten.
    public func setOnline(_ online: Bool) { signal(["online": .bool(online)]) }

    /// What an app calls as it comes back to the foreground: the streams a
    /// suspension cut are opened again at once, from their cursors.
    public func resume() { setOnline(true) }

    /// The server itself, for what the replica does not hold.
    public var remote: FenecRemote {
        lock.lock()
        defer { lock.unlock() }
        return FenecRemote(url: url, token: token)
    }

    // ---------------------------------------------------------- the engine

    func start(_ first: [UInt8]) {
        queue.async { self.perform(first) }
    }

    /// Asks for what a write left due.
    func poll() {
        queue.async { self.feed(UInt32(FENEC_SYNC_POLL), 0) }
    }

    func stop() {
        queue.sync {
            guard !stopped else { return }
            feed(UInt32(FENEC_SYNC_SIGNAL), 0, bytes: Array(#"{"stop":true}"#.utf8))
            stopped = true
            for t in tasks.values { t.cancel() }
            tasks.removeAll()
            session.invalidateAndCancel()
            lock.lock()
            watchers.values.forEach { $0.finish() }
            refusalWatchers.values.forEach { $0.finish() }
            watchers.removeAll()
            refusalWatchers.removeAll()
            lock.unlock()
        }
    }

    private func signal(_ fields: Row) {
        let bytes = Array(Value.object(fields).json.utf8)
        queue.async { self.feed(UInt32(FENEC_SYNC_SIGNAL), 0, bytes: bytes) }
    }

    /// Tells the engine, on the queue, and performs what it answers.
    private func feed(_ kind: UInt32, _ id: UInt64, status: Int = 0, seq: UInt64 = 0, bytes: [UInt8] = []) {
        if stopped { return }
        let out = try? Fenec.call { out, len in
            bytes.withUnsafeBufferPointer { b in
                fenec_sync_feed(handle, kind, id, Int32(clamping: status), seq, b.baseAddress, b.count, out, len)
            }
        }
        if let out { perform(out) }
    }

    private func perform(_ bytes: [UInt8]) {
        guard let actions = (try? Value.parse(bytes: bytes))?.array else { return }
        for a in actions {
            let id = UInt64(a["id"]?.int ?? 0)
            switch a["do"]?.string {
            case "request": request(id, a)
            case "stream": stream(id, a)
            case "cancel": tasks.removeValue(forKey: id)?.cancel()
            case "wait":
                let ms = a["ms"]?.int ?? 0
                queue.asyncAfter(deadline: .now() + .milliseconds(ms)) { self.feed(UInt32(FENEC_SYNC_TIMER), id) }
            case "token":
                if let provider {
                    Task {
                        if let t = try? await provider() { self.setToken(t) }
                    }
                }
            case "changed":
                if let db { db.lives.touch(db) }
            case "refused":
                let r = Refusal(
                    status: a["status"]?.int ?? 0, message: a["message"]?.string ?? "", query: a["query"]?.string ?? "")
                lock.lock()
                let ws = Array(refusalWatchers.values)
                lock.unlock()
                ws.forEach { $0.yield(r) }
            case "status": reload()
            default: break
            }
        }
    }

    private func reload() {
        guard let out = try? Fenec.call({ out, len in fenec_sync_status(handle, out, len) }),
            let v = try? Value.parse(bytes: out)
        else { return }
        let s = SyncStatus(v)
        lock.lock()
        current = s
        let ws = Array(watchers.values)
        lock.unlock()
        ws.forEach { $0.yield(s) }
    }

    private func urlRequest(_ a: Value) -> URLRequest? {
        guard let text = a["url"]?.string, let u = URL(string: text) else { return nil }
        var req = URLRequest(url: u)
        req.httpMethod = a["method"]?.string ?? "GET"
        for (k, v) in a["headers"]?.object ?? Row() {
            req.setValue(v.string, forHTTPHeaderField: k)
        }
        if let body = a["body"]?.string { req.httpBody = Data(body.utf8) }
        return req
    }

    private func request(_ id: UInt64, _ a: Value) {
        guard let req = urlRequest(a) else {
            return feed(UInt32(FENEC_SYNC_RESPONSE), id, bytes: Array("a request the platform cannot make".utf8))
        }
        let task = session.dataTask(with: req) { [weak self] data, response, error in
            guard let self else { return }
            self.tasks[id] = nil
            let http = response as? HTTPURLResponse
            let status = error == nil ? (http?.statusCode ?? 0) : 0
            let seq = UInt64(http?.value(forHTTPHeaderField: "Fenec-Seq") ?? "") ?? 0
            let body = error.map { Array($0.localizedDescription.utf8) } ?? Array(data ?? Data())
            self.feed(UInt32(FENEC_SYNC_RESPONSE), id, status: status, seq: seq, bytes: body)
        }
        tasks[id] = task
        task.resume()
    }

    private func stream(_ id: UInt64, _ a: Value) {
        guard let req = urlRequest(a) else {
            return feed(UInt32(FENEC_SYNC_CLOSED), id, bytes: Array("a stream the platform cannot open".utf8))
        }
        let task = session.dataTask(with: req)
        tasks[id] = task
        streams[task.taskIdentifier] = (id, 0, Data())
        task.resume()
    }

    // ------------------------------------------------- URLSession's events

    public func urlSession(
        _ session: URLSession, dataTask: URLSessionDataTask, didReceive response: URLResponse,
        completionHandler: @escaping (URLSession.ResponseDisposition) -> Void
    ) {
        if var s = streams[dataTask.taskIdentifier] {
            s.status = (response as? HTTPURLResponse)?.statusCode ?? 0
            streams[dataTask.taskIdentifier] = s
            if s.status == 200 { feed(UInt32(FENEC_SYNC_OPENED), s.id, status: 200) }
        }
        completionHandler(.allow)
    }

    public func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive data: Data) {
        guard var s = streams[dataTask.taskIdentifier] else { return }
        if s.status == 200 {
            feed(UInt32(FENEC_SYNC_BYTES), s.id, bytes: Array(data))
        } else {
            s.body.append(data)
            streams[dataTask.taskIdentifier] = s
        }
    }

    public func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        guard let s = streams.removeValue(forKey: task.taskIdentifier) else { return }
        tasks[s.id] = nil
        if s.status != 0 && s.status != 200 {
            feed(UInt32(FENEC_SYNC_OPENED), s.id, status: s.status, bytes: Array(s.body))
        } else {
            feed(UInt32(FENEC_SYNC_CLOSED), s.id, bytes: Array((error?.localizedDescription ?? "").utf8))
        }
    }
}
