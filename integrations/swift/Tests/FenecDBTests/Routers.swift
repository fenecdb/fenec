// What the read-routing tests (ReadsTests.swift) need of Foundation, in a
// file of its own as Servers.swift is: a tenant with a read replica behind
// real routers, and a URLSession whose requests are kept -- passed on to
// the routers, or answered from a script.
#if os(macOS)
    import Foundation

    @testable import FenecDB

    /// eu1 in eu and us1 in us, the global router near eu and its standby
    /// near us; tenant `acme` on eu1 with its read replica on us1. A client
    /// in us asks the router there: `url`.
    final class ReadReplicas: @unchecked Sendable {
        let eu1: Daemon
        let us1: Daemon
        private let routers: [Daemon]
        private let dir: URL
        /// The tenant through us's router.
        let url: String

        private init(eu1: Daemon, us1: Daemon, routers: [Daemon], dir: URL, url: String) {
            self.eu1 = eu1
            self.us1 = us1
            self.routers = routers
            self.dir = dir
            self.url = url
        }

        static func start(_ name: String) async throws -> ReadReplicas {
            let dir = URL(fileURLWithPath: try scratch("replicas-\(name)")).deletingLastPathComponent()
            let server = Daemon.binary("fenec-server", env: "FENEC_SERVER")
            let shard = Daemon.binary("fenec-shard", env: "FENEC_SHARD")
            var started: [Daemon] = []
            do {
                func node(_ n: String) async throws -> Daemon {
                    let d = try await Daemon.start(
                        server,
                        [
                            "--dir", dir.appendingPathComponent(n).path, "--http", "127.0.0.1:0",
                            "--admin-token", "adm", "--replication-token", "repl",
                        ])
                    started.append(d)
                    return d
                }
                func router(_ file: String, _ extra: [String]) async throws -> Daemon {
                    let d = try await Daemon.start(
                        shard,
                        [
                            "--listen", "127.0.0.1:0", "--directory", dir.appendingPathComponent(file).path,
                            "--token", "root", "--replication-token", "dirs",
                        ] + extra)
                    started.append(d)
                    return d
                }
                let eu1 = try await node("eu1")
                let us1 = try await node("us1")
                let eu = try await router("global.fenec", ["--near", "eu"])
                let us = try await router("standby.fenec", ["--near", "us", "--replica-of", "http://127.0.0.1:\(eu.port)"])
                let global = "http://127.0.0.1:\(eu.port)"
                _ = try await call(global, "PUT", "/_shard/nodes/eu1", ["addr": "127.0.0.1:\(eu1.port)", "token": "adm", "region": "eu"])
                _ = try await call(global, "PUT", "/_shard/nodes/us1", ["addr": "127.0.0.1:\(us1.port)", "token": "adm", "region": "us"])
                _ = try await call(global, "PUT", "/_shard/tenants/acme", ["node": "eu1", "schema": "create collection notes (body text)"])
                let placed = try await call(global, "PUT", "/_shard/tenants/acme/read-replicas/us", nil)
                guard placed["node"] as? String == "us1" else {
                    throw FenecError(code: .io, message: "the read replica was placed elsewhere: \(placed)")
                }
                // us's router follows the global directory: it routes to the
                // replica once its copy holds it.
                let near = "http://127.0.0.1:\(us.port)"
                for _ in 0..<3000 {
                    let list = try await call(near, "GET", "/_shard/tenants/acme/read-replicas", nil)
                    if let rs = list["read_replicas"] as? [[String: Any]], rs.contains(where: { $0["node"] as? String == "us1" }) {
                        return ReadReplicas(eu1: eu1, us1: us1, routers: [eu, us], dir: dir, url: "\(near)/t/acme")
                    }
                    try await Task.sleep(nanoseconds: 20_000_000)
                }
                throw FenecError(code: .io, message: "us's router never knew of the read replica")
            } catch {
                started.forEach { $0.stop() }
                throw error
            }
        }

        /// The rows the replica on us1 holds, asked of its node directly.
        func replicaCount() async throws -> Int? {
            var req = URLRequest(url: URL(string: "http://127.0.0.1:\(us1.port)/t/acme/query")!)
            req.httpMethod = "POST"
            req.setValue("replica", forHTTPHeaderField: "fenec-read")
            req.httpBody = Data(#"{"query":"get notes count"}"#.utf8)
            let (data, res) = try await URLSession.shared.data(for: req)
            guard (res as? HTTPURLResponse)?.statusCode == 200,
                let rows = try JSONSerialization.jsonObject(with: data) as? [[String: Any]]
            else { return nil }
            return rows.first?["count"] as? Int
        }

        /// Everything killed, the files let go of.
        func stop() {
            for d in routers + [us1, eu1] { d.stop() }
            try? FileManager.default.removeItem(at: dir)
        }

        /// A router's admin call under its token: the JSON it answers.
        private static func call(_ base: String, _ method: String, _ path: String, _ body: [String: String]?) async throws
            -> [String: Any]
        {
            var req = URLRequest(url: URL(string: base + path)!)
            req.httpMethod = method
            req.setValue("Bearer root", forHTTPHeaderField: "authorization")
            if let body { req.httpBody = try JSONSerialization.data(withJSONObject: body) }
            let (data, res) = try await URLSession.shared.data(for: req)
            let status = (res as? HTTPURLResponse)?.statusCode ?? 0
            guard (200..<300).contains(status) else {
                throw FenecError(code: .io, message: "\(method) \(path): \(status) \(String(decoding: data, as: UTF8.self))")
            }
            return data.isEmpty ? [:] : (try JSONSerialization.jsonObject(with: data) as? [String: Any]) ?? [:]
        }
    }

    /// One answer of a script: its status, body and head, given once `gate`
    /// is set where it has one -- two requests under way at once.
    struct Scripted: Sendable {
        var status = 200
        var body = "[]"
        var seq: String? = nil
        var history: String? = nil
        var gate: Once<Bool>? = nil
    }

    /// A URLSession whose requests' heads are kept, in the order they went:
    /// each passed on as it was, or -- given a script -- answered from it
    /// in turn, an empty list's `[]` once it runs out. Kept as each request
    /// starts loading, before its answer comes back.
    final class Wire: URLProtocol, @unchecked Sendable {
        final class Log: @unchecked Sendable {
            private let lock = NSLock()
            private var heads: [[String: String]] = []
            private var script: [Scripted]
            fileprivate let scripted: Bool

            init(script: [Scripted]?) {
                self.script = script ?? []
                scripted = script != nil
            }

            /// Every request's head so far, its names in lower case.
            var all: [[String: String]] {
                lock.lock()
                defer { lock.unlock() }
                return heads
            }

            /// The last request's head.
            var last: [String: String] { all.last ?? [:] }

            /// Answers for the script's end.
            func add(_ more: Scripted...) {
                lock.lock()
                script += more
                lock.unlock()
            }

            fileprivate func took(_ head: [String: String]) -> Scripted {
                lock.lock()
                defer { lock.unlock() }
                heads.append(Dictionary(head.map { ($0.key.lowercased(), $0.value) }, uniquingKeysWith: { a, _ in a }))
                return script.isEmpty ? Scripted() : script.removeFirst()
            }
        }

        // The log a request goes to, by the host and port it is sent to:
        // each test's are its own -- a router's port, a script's name.
        private static let lock = NSLock()
        private static var logs: [String: Log] = [:]

        private static func key(_ url: URL?) -> String { "\(url?.host ?? ""):\(url?.port ?? 0)" }

        /// A session whose requests to `url` go to `log`, passed on or
        /// scripted.
        static func session(_ url: String, _ log: Log) -> URLSession {
            lock.lock()
            logs[key(URL(string: url))] = log
            lock.unlock()
            let c = URLSessionConfiguration.ephemeral
            c.protocolClasses = [Wire.self]
            return URLSession(configuration: c)
        }

        override class func canInit(with request: URLRequest) -> Bool { true }
        override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

        private var forwarded: URLSessionDataTask?

        override func startLoading() {
            var req = request
            Wire.lock.lock()
            let log = Wire.logs[Wire.key(req.url)]
            Wire.lock.unlock()
            guard let log else {
                client?.urlProtocol(self, didFailWithError: URLError(.unsupportedURL))
                return
            }
            let answer = log.took(req.allHTTPHeaderFields ?? [:])
            if log.scripted {
                let deliver = { [self] in
                    var head = ["content-type": "application/json"]
                    if let s = answer.seq { head["fenec-seq"] = s }
                    if let h = answer.history { head["fenec-history"] = h }
                    let res = HTTPURLResponse(url: req.url!, statusCode: answer.status, httpVersion: "HTTP/1.1", headerFields: head)!
                    client?.urlProtocol(self, didReceive: res, cacheStoragePolicy: .notAllowed)
                    client?.urlProtocol(self, didLoad: Data(answer.body.utf8))
                    client?.urlProtocolDidFinishLoading(self)
                }
                if let gate = answer.gate {
                    Task {
                        _ = await gate.value()
                        deliver()
                    }
                } else {
                    deliver()
                }
                return
            }
            // A URLProtocol is handed the body as a stream.
            if req.httpBody == nil, let stream = req.httpBodyStream {
                var body = Data()
                var chunk = [UInt8](repeating: 0, count: 4096)
                stream.open()
                while stream.hasBytesAvailable {
                    let n = stream.read(&chunk, maxLength: chunk.count)
                    if n <= 0 { break }
                    body.append(chunk, count: n)
                }
                stream.close()
                req.httpBodyStream = nil
                req.httpBody = body
            }
            forwarded = URLSession.shared.dataTask(with: req) { [self] data, response, error in
                if let error {
                    client?.urlProtocol(self, didFailWithError: error)
                    return
                }
                client?.urlProtocol(self, didReceive: response!, cacheStoragePolicy: .notAllowed)
                if let data { client?.urlProtocol(self, didLoad: data) }
                client?.urlProtocolDidFinishLoading(self)
            }
            forwarded?.resume()
        }

        override func stopLoading() { forwarded?.cancel() }
    }

    /// A connection that reads `reads`, its requests kept in `log`.
    func connection(_ url: String, reads: FenecRemote.Reads, log: Wire.Log) -> FenecRemote {
        FenecRemote(url: url, token: nil, reads: reads, session: Wire.session(url, log))
    }
#endif
