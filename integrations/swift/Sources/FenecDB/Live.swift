import Foundation

#if canImport(Combine)
    import Combine
#endif
#if canImport(Observation)
    import Observation
#endif

/// The live queries over one database, as `web/fenec.js`'s `Lives` keeps a
/// page's: the change ring says which collections were written since a
/// cursor (`changes(since:)`), a block's once it lands whole, so knowing
/// costs a write nothing -- it is asked once after a burst of writes, and
/// only by a database holding a live query. Collection granularity: a
/// query that reads what was written runs again from scratch, which over a
/// local file is well under a millisecond, and anything finer would cost
/// more than the query.
///
/// Everything here is on the main actor, where a SwiftUI view reads the
/// rows. A write through the database asks for a look (`touch`); the looks
/// a burst asks for are one, taken a frame later and once no write is under
/// way, so a loop of writes or several at once run each query once.
@MainActor
final class Lives {
    /// `on` is read and written on the main actor alone; the rest is set
    /// once.
    final class Sub: @unchecked Sendable {
        let rows: @Sendable () async throws -> [Row]
        let reads: Set<String>?
        let deliver: @MainActor ([Row]) -> Void
        let fail: @MainActor (Error) -> Void
        var on = true

        init(
            rows: @escaping @Sendable () async throws -> [Row], reads: Set<String>?,
            deliver: @escaping @MainActor ([Row]) -> Void, fail: @escaping @MainActor (Error) -> Void
        ) {
            self.rows = rows
            self.reads = reads
            self.deliver = deliver
            self.fail = fail
        }
    }

    /// How long a burst's looks are gathered: a frame at 60 Hz, the
    /// soonest a view shows anything anyway.
    static let gather: UInt64 = 16_000_000

    private var subs: [Sub] = []
    private var cursor: UInt64 = 0
    private var due = false
    /// Runs every query at the next look, whatever the ring says.
    private var all = false

    nonisolated init() {}

    /// Called from any thread after a write: one look, however many ask.
    nonisolated func touch(_ db: Fenec) {
        Task { @MainActor in self.schedule(db) }
    }

    private func schedule(_ db: Fenec) {
        guard !subs.isEmpty, !due else { return }
        due = true
        Task { @MainActor in
            try? await Task.sleep(nanoseconds: Lives.gather)
            // A write under way asks again as it ends.
            due = false
            if db.writing { return }
            await tick(db)
        }
    }

    func add(_ sub: Sub, db: Fenec) async {
        // With none before it no look has kept the cursor up: it starts
        // here, where the first run reads.
        if subs.isEmpty, let c = try? await db.changes(since: .max) { cursor = c.seq }
        subs.append(sub)
        await run(sub)
    }

    func remove(_ sub: Sub) {
        sub.on = false
        subs.removeAll { $0 === sub }
    }

    func clear() {
        subs.forEach { $0.on = false }
        subs.removeAll()
    }

    private func tick(_ db: Fenec) async {
        guard let info = try? await db.changes(since: cursor) else { return }
        cursor = info.seq
        let dirty = all || info.collections == nil ? nil : Set(info.collections!)
        all = false
        // Only reads since: nothing to run.
        if dirty?.isEmpty == true { return }
        for s in subs {
            if let dirty, let reads = s.reads, reads.isDisjoint(with: dirty) { continue }
            await run(s)
        }
    }

    private func run(_ s: Sub) async {
        do {
            let rows = try await s.rows()
            // Stopped while it ran: its rows go nowhere.
            if s.on { s.deliver(rows) }
        } catch {
            if s.on { s.fail(error) }
        }
    }
}

/// Where an observable keeps its live query, for its `deinit` -- which runs
/// off the main actor -- to stop it.
final class HandleBox: @unchecked Sendable {
    var live: LiveHandle?
}

/// What stops a live query.
public final class LiveHandle: @unchecked Sendable {
    private let stopper: @Sendable () -> Void
    init(_ stop: @escaping @Sendable () -> Void) { stopper = stop }
    public func stop() { stopper() }
}

extension Fenec {
    /// A live query: `deliver` handed its rows now and again after every
    /// write to a collection it reads, on the main actor. A builder query
    /// knows the collections it reads; a text names them with
    /// `collections`, or runs again after every write.
    @discardableResult
    func subscribe(
        _ target: LiveTarget,
        deliver: @escaping @MainActor ([Row]) -> Void,
        fail: @escaping @MainActor (Error) -> Void
    ) -> LiveHandle {
        let sub = Lives.Sub(rows: target.rows(on: self), reads: target.reads, deliver: deliver, fail: fail)
        let lives = self.lives
        Task { @MainActor in await lives.add(sub, db: self) }
        return LiveHandle { Task { @MainActor in lives.remove(sub) } }
    }

    /// The rows of `query` now, and again after every write to a collection
    /// it reads, on the main actor. An error ends the stream; so does
    /// letting go of it.
    public func live(_ query: Query) -> AsyncThrowingStream<[Row], Error> {
        stream(LiveTarget(query: query))
    }

    /// `live`, each row decoded as `T`.
    public func live<T: Decodable & Sendable>(_ query: Query, as type: T.Type) -> AsyncThrowingStream<[T], Error> {
        let rows = stream(LiveTarget(query: query))
        return AsyncThrowingStream { cont in
            let task = Task {
                do {
                    for try await r in rows { cont.yield(try decoded(r) as [T]) }
                    cont.finish()
                } catch { cont.finish(throwing: error) }
            }
            cont.onTermination = { _ in task.cancel() }
        }
    }

    /// A live FenecQL text: run again after every write to `collections`,
    /// or after every write when it names none.
    public func live(_ text: String, _ params: any FenecValue..., collections: [String]? = nil) throws
        -> AsyncThrowingStream<[Row], Error>
    {
        stream(LiveTarget(text: text, params: try params.map { try $0.fenecValue() }, reads: collections.map(Set.init)))
    }

    private func stream(_ target: LiveTarget) -> AsyncThrowingStream<[Row], Error> {
        AsyncThrowingStream { cont in
            let handle = subscribe(target, deliver: { cont.yield($0) }, fail: { cont.finish(throwing: $0) })
            cont.onTermination = { _ in handle.stop() }
        }
    }
}

/// What a live query runs: a builder query, or a text with its
/// parameters, and the collections it reads.
struct LiveTarget: Sendable {
    let text: String
    let params: [Value]
    let reads: Set<String>?

    init(query: Query) {
        // Its text made now: a chain the builder refuses is the live
        // query's first error, delivered where its rows would have been.
        do {
            (text, params) = try query.toFenecQL()
            failure = nil
        } catch {
            (text, params) = ("", [])
            failure = error as? FenecError ?? .builder("\(error)")
        }
        reads = query.reads.map(Set.init)
    }

    init(text: String, params: [Value], reads: Set<String>?) {
        self.text = text
        self.params = params
        self.reads = reads
        failure = nil
    }

    let failure: FenecError?

    func rows(on db: Fenec) -> @Sendable () async throws -> [Row] {
        let (text, params, failure) = (self.text, self.params, self.failure)
        return { [weak db] in
            if let failure { throw failure }
            guard let db else { return [] }
            return try await db.run(text, params: params, quiet: true).rows
        }
    }
}

#if canImport(Combine)
    /// A live query for SwiftUI as an `ObservableObject`: `rows` now and again
    /// after every write to a collection the query reads, set on the main
    /// actor, the writes of a burst gathered into one run.
    ///
    ///     @StateObject var todos = LiveQuery(db, try db.from("todos").where("done", false), as: Todo.self)
    ///     ... List(todos.rows) { Text($0.title) }
    @MainActor
    public final class LiveQuery<Element: Sendable>: ObservableObject {
        @Published public private(set) var rows: [Element] = []
        /// The last run's error, cleared by the next run that succeeds.
        @Published public private(set) var error: FenecError?
        /// Whether the first rows came.
        @Published public private(set) var loaded = false
        private let handle = HandleBox()

        public convenience init(_ db: Fenec, _ query: Query) where Element == Row {
            self.init(db, LiveTarget(query: query)) { $0 }
        }

        public convenience init(_ db: Fenec, _ query: Query, as type: Element.Type) where Element: Decodable {
            self.init(db, LiveTarget(query: query)) { try decoded($0) }
        }

        /// A FenecQL text, run again after every write to `collections`, or
        /// to anything when it names none.
        public convenience init(_ db: Fenec, _ text: String, params: [Value] = [], collections: [String]? = nil)
        where Element == Row {
            self.init(db, LiveTarget(text: text, params: params, reads: collections.map(Set.init))) { $0 }
        }

        init(_ db: Fenec, _ target: LiveTarget, map: @escaping ([Row]) throws -> [Element]) {
            handle.live = db.subscribe(
                target,
                deliver: { [weak self] rows in
                    guard let self else { return }
                    do {
                        self.rows = try map(rows)
                        self.error = nil
                    } catch {
                        self.error = FenecError(code: .type, message: "\(error)")
                    }
                    self.loaded = true
                },
                fail: { [weak self] e in
                    self?.error = e as? FenecError ?? FenecError(code: .query, message: "\(e)")
                    self?.loaded = true
                })
        }

        /// Stops it: no more runs.
        public func stop() { handle.live?.stop() }

        deinit { handle.live?.stop() }
    }
#endif

#if canImport(Observation)
    /// `LiveQuery` for the Observation framework (`@Observable`): what a
    /// view on iOS 17 or macOS 14 holds in `@State`.
    @available(macOS 14, iOS 17, tvOS 17, watchOS 10, *)
    @MainActor
    @Observable
    public final class LiveRows<Element: Sendable> {
        public private(set) var rows: [Element] = []
        public private(set) var error: FenecError?
        public private(set) var loaded = false
        @ObservationIgnored private let handle = HandleBox()

        public convenience init(_ db: Fenec, _ query: Query) where Element == Row {
            self.init(db, LiveTarget(query: query)) { $0 }
        }

        public convenience init(_ db: Fenec, _ query: Query, as type: Element.Type) where Element: Decodable {
            self.init(db, LiveTarget(query: query)) { try decoded($0) }
        }

        init(_ db: Fenec, _ target: LiveTarget, map: @escaping ([Row]) throws -> [Element]) {
            handle.live = db.subscribe(
                target,
                deliver: { [weak self] rows in
                    guard let self else { return }
                    do {
                        self.rows = try map(rows)
                        self.error = nil
                    } catch {
                        self.error = FenecError(code: .type, message: "\(error)")
                    }
                    self.loaded = true
                },
                fail: { [weak self] e in
                    self?.error = e as? FenecError ?? FenecError(code: .query, message: "\(e)")
                    self?.loaded = true
                })
        }

        public func stop() { handle.live?.stop() }

        deinit { handle.live?.stop() }
    }
#endif
