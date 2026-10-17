// A fenec-server the sync tests start, kill and start again: the binary
// `cargo build -p fenec-server` makes (FENEC_SERVER names another). In a
// file of its own, since it needs Foundation and a file importing both
// Foundation and Testing needs an overlay the Command Line Tools lack.
#if os(macOS)
    import Foundation

    @testable import FenecDB

    final class Server: @unchecked Sendable {
        let file: String
        /// A tenant node's quota of rows a tenant, its tasks in tenant
        /// `acme`; nil for a server over a file.
        private let tenantRows: Int?
        private(set) var port = 0
        private var daemon: Daemon?

        var url: String { "http://127.0.0.1:\(port)" + (tenantRows == nil ? "" : "/t/acme") }

        static var binary: String { Daemon.binary("fenec-server", env: "FENEC_SERVER") }

        /// A server over a file of its own, holding `tasks` -- or, given
        /// `tenantRows`, a tenant node (`--dir`) holding each tenant to that
        /// many rows, `tasks` in its tenant `acme`, which `url` names.
        init(_ name: String, tenantRows: Int? = nil) async throws {
            file = try scratch("server-\(name)")
            self.tenantRows = tenantRows
            try await start(port: 0)
            if tenantRows != nil {
                var req = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/_admin/tenants/acme")!)
                req.httpMethod = "PUT"
                req.setValue("Bearer adm", forHTTPHeaderField: "authorization")
                let (_, res) = try await URLSession.shared.data(for: req)
                guard (res as? HTTPURLResponse)?.statusCode == 201 else {
                    throw FenecError(code: .io, message: "the tenant was not made: \(res)")
                }
            }
            try await run(
                "create collection tasks (key text @unique, title text, status text @hash, priority int)")
            try await run(
                #"put tasks [{key: "a", title: "one", status: "open", priority: 1}, {key: "b", title: "two", status: "open", priority: 5}, {key: "c", title: "three", status: "closed", priority: 3}]"#
            )
        }

        /// Started on `port`, 0 for one of its own; waits for it to say where.
        func start(port: Int) async throws {
            let d = try await Daemon.start(
                Server.binary,
                ["--http", "127.0.0.1:\(port)", "--sync", "always"]
                    + (tenantRows.map { ["--dir", file, "--admin-token", "adm", "--tenant-max-rows", "\($0)"] }
                        ?? ["--file", file]))
            daemon = d
            self.port = d.port
        }

        /// Killed, as a crash would: no checkpoint, no goodbye. Returns once
        /// it is gone, its file and its port let go of.
        func kill() async {
            let d = daemon
            daemon = nil
            await d?.kill()
        }

        /// Started again over its file, on the port it had.
        func restart() async throws { try await start(port: port) }

        /// Killed and not waited for: what a test's `defer` can do.
        func stop() {
            daemon?.stop()
            daemon = nil
        }

        /// A statement on the server.
        @discardableResult
        func run(_ text: String) async throws -> Answer {
            try await Fenec.connect(url: url).run(text, params: [])
        }
    }

    /// A server binary started on a port of its own -- fenec-server's,
    /// fenec-shard's -- found by the address its `listening on:` line
    /// names, and killed.
    ///
    /// Nothing here holds a thread of Swift's cooperative pool, which has
    /// a thread a core -- three on GitHub's macOS runner. Its stderr is
    /// read on a thread of its own until it closes, and its end is
    /// awaited through `terminationHandler`. A kill was reaped with
    /// `waitUntilExit()`, which waits in the calling thread's run loop:
    /// called from a task on a process with no `terminationHandler`,
    /// it now and then never came back -- a program doing only that hung
    /// within 200 kills in three runs of four, the pool cut to one
    /// thread or not -- and the run hung for hours. And a
    /// `readabilityHandler` is called again and again once its pipe has
    /// ended: each server killed left a thread spinning for the rest of
    /// the run, five by the last tests.
    final class Daemon: @unchecked Sendable {
        let port: Int
        private let process: Process
        private let ended: Once<Bool>

        private init(_ process: Process, _ ended: Once<Bool>, port: Int) {
            self.process = process
            self.ended = ended
            self.port = port
        }

        /// `name` as `cargo build` made it at the repository's root, or as
        /// `env` names it.
        static func binary(_ name: String, env: String) -> String {
            if let b = ProcessInfo.processInfo.environment[env] { return b }
            let root = URL(fileURLWithPath: #filePath)
                .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
                .deletingLastPathComponent().deletingLastPathComponent()
            return root.appendingPathComponent("target/debug/\(name)").path
        }

        /// Started with `args`; waits for it to say where it listens.
        static func start(_ binary: String, _ args: [String]) async throws -> Daemon {
            let name = URL(fileURLWithPath: binary).lastPathComponent
            let p = Process()
            p.executableURL = URL(fileURLWithPath: binary)
            p.arguments = args
            let err = Pipe()
            p.standardError = err
            p.standardOutput = FileHandle.nullDevice
            let exit = Once<Bool>()
            p.terminationHandler = { _ in exit.set(true) }
            let listening = Once<Result<Int, FenecError>>()
            try p.run()
            let reader = err.fileHandleForReading
            let thread = Thread {
                var text = ""
                var chunk = [UInt8](repeating: 0, count: 4096)
                while true {
                    let n = Darwin.read(reader.fileDescriptor, &chunk, chunk.count)
                    if n < 0 && errno == EINTR { continue }
                    if n <= 0 { break }
                    // Past the address, the rest of its output goes nowhere.
                    if listening.done { continue }
                    text += String(decoding: chunk[0..<n], as: UTF8.self)
                    if let r = text.range(of: "listening on: http://127.0.0.1:") {
                        let digits = text[r.upperBound...].prefix { $0.isNumber }
                        if let n = Int(digits), text[r.upperBound...].contains(" ") {
                            listening.set(.success(n))
                        }
                    }
                }
                try? reader.close()
                listening.set(.failure(FenecError(code: .io, message: "\(name) did not start: \(text)")))
            }
            thread.start()
            let timeout = Task {
                try await Task.sleep(nanoseconds: 20_000_000_000)
                listening.set(.failure(FenecError(code: .io, message: "\(name) did not say where in 20 s")))
            }
            defer { timeout.cancel() }
            switch await listening.value() {
            case .success(let n): return Daemon(p, exit, port: n)
            case .failure(let e):
                Darwin.kill(p.processIdentifier, SIGKILL)
                _ = await exit.value()
                throw e
            }
        }

        /// Killed, as a crash would: no checkpoint, no goodbye. Returns once
        /// it is gone, its files and its port let go of.
        func kill() async {
            Darwin.kill(process.processIdentifier, SIGKILL)
            _ = await ended.value()
        }

        /// Killed and not waited for: what a test's `defer` can do.
        func stop() { Darwin.kill(process.processIdentifier, SIGKILL) }
    }

    /// A value set once, which tasks wait for without holding a thread.
    final class Once<T: Sendable>: @unchecked Sendable {
        private let lock = NSLock()
        private var value: T?
        private var waiting: [CheckedContinuation<T, Never>] = []

        var done: Bool {
            lock.lock()
            defer { lock.unlock() }
            return value != nil
        }

        /// The first set wins.
        func set(_ v: T) {
            lock.lock()
            guard value == nil else { return lock.unlock() }
            value = v
            let ws = waiting
            waiting = []
            lock.unlock()
            ws.forEach { $0.resume(returning: v) }
        }

        func value() async -> T {
            await withCheckedContinuation { cont in
                lock.lock()
                if let v = value {
                    lock.unlock()
                    cont.resume(returning: v)
                } else {
                    waiting.append(cont)
                    lock.unlock()
                }
            }
        }
    }

    /// A lock, for the tests' files that cannot name Foundation's.
    final class NSLockish: @unchecked Sendable {
        private let lock = NSLock()
        func with<T>(_ f: () -> T) -> T {
            lock.lock()
            defer { lock.unlock() }
            return f()
        }
    }
#endif
