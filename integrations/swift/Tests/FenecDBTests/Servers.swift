// A fenec-server the sync tests start, kill and start again: the binary
// `cargo build -p fenec-server` makes (FENEC_SERVER names another). In a
// file of its own, since it needs Foundation and a file importing both
// Foundation and Testing needs an overlay the Command Line Tools lack.
#if os(macOS)
    import Foundation

    @testable import FenecDB

    final class Server: @unchecked Sendable {
        let file: String
        private(set) var port = 0
        private var process: Process?

        var url: String { "http://127.0.0.1:\(port)" }

        static var binary: String {
            if let b = ProcessInfo.processInfo.environment["FENEC_SERVER"] { return b }
            let root = URL(fileURLWithPath: #filePath)
                .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
                .deletingLastPathComponent().deletingLastPathComponent()
            return root.appendingPathComponent("target/debug/fenec-server").path
        }

        /// A server over a file of its own, holding `tasks`.
        init(_ name: String) async throws {
            file = try scratch("server-\(name)")
            try start(port: 0)
            try await run(
                "create collection tasks (key text @unique, title text, status text @hash, priority int)")
            try await run(
                #"put tasks [{key: "a", title: "one", status: "open", priority: 1}, {key: "b", title: "two", status: "open", priority: 5}, {key: "c", title: "three", status: "closed", priority: 3}]"#
            )
        }

        /// Started on `port`, 0 for one of its own; waits for it to say where.
        func start(port: Int) throws {
            let p = Process()
            p.executableURL = URL(fileURLWithPath: Server.binary)
            p.arguments = ["--http", "127.0.0.1:\(port)", "--file", file, "--sync", "always"]
            let err = Pipe()
            p.standardError = err
            p.standardOutput = FileHandle.nullDevice
            try p.run()
            process = p
            var text = ""
            let deadline = Date().addingTimeInterval(20)
            while Date() < deadline {
                let chunk = err.fileHandleForReading.availableData
                if chunk.isEmpty { break }
                text += String(decoding: chunk, as: UTF8.self)
                if let r = text.range(of: "listening on: http://127.0.0.1:") {
                    let digits = text[r.upperBound...].prefix { $0.isNumber }
                    if let n = Int(digits), text[r.upperBound...].contains(" ") {
                        self.port = n
                        // The rest of its output goes nowhere.
                        err.fileHandleForReading.readabilityHandler = { _ = $0.availableData }
                        return
                    }
                }
            }
            p.terminate()
            throw FenecError(code: .io, message: "fenec-server did not start: \(text)")
        }

        /// Killed, as a crash would: no checkpoint, no goodbye.
        func kill() {
            guard let p = process else { return }
            Darwin.kill(p.processIdentifier, SIGKILL)
            p.waitUntilExit()
            process = nil
        }

        /// Started again over its file, on the port it had.
        func restart() throws { try start(port: port) }

        func stop() { kill() }

        /// A statement on the server.
        @discardableResult
        func run(_ text: String) async throws -> Answer {
            try await Fenec.connect(url: url).run(text, params: [])
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
