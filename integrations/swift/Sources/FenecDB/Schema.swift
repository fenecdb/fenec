import FenecFFI
import Foundation

/// A difference no open applies -- one that would lose data or could mean
/// two things -- and how to resolve it, as the engine writes them.
public struct SchemaRefusal: Sendable, Equatable {
    public let kind: String
    public let collection: String
    /// The field, or the path into a json field; `nil` for the collection.
    public let field: String?
    public let message: String
    public let fix: String
}

/// What the engine found comparing the database with the schema the app
/// declares, and did: the FenecQL that adds what is missing, the
/// migrations it recorded, and what it refused.
public struct SchemaPlan: Sendable, Equatable {
    public let applied: Bool
    /// Whether the migrations recorded ran: a database made from the schema
    /// records them without running them.
    public let ran: Bool
    public let migrations: [Int]
    public let statements: [String]
    public let refusals: [SchemaRefusal]
}

/// A schema the database differs from in what no open applies.
public struct SchemaError: Error, CustomStringConvertible, Sendable {
    public let refusals: [SchemaRefusal]
    public var description: String {
        "the database's schema differs from the app's:"
            + refusals.map { "\n  - \($0.message)\n    \($0.fix)" }.joined()
    }
}

extension Fenec {
    /// Brings the database to `fenecql` -- `create collection` and `create
    /// index` statements, a `schema.fenecql` file -- as every SDK does, in
    /// the engine: the migrations not yet recorded run first, in order, and
    /// what only adds is made, all one block. Anything that would lose data
    /// or could mean two things throws a `SchemaError` naming each
    /// difference, and nothing is written. With `apply: false`, what an
    /// apply would do.
    @discardableResult
    public func schema(_ fenecql: String, migrations: [String] = [], apply: Bool = true) async throws -> SchemaPlan {
        if isClosed { throw FenecError(code: .misuse, message: "the database was closed") }
        let request = Value.object([
            "format": .int(1), "fenecql": .string(fenecql), "migrations": .array(migrations.map { .string($0) }),
        ]).json
        let handle = self.handle
        let mode: UInt32 = apply ? UInt32(FENEC_SCHEMA_APPLY) : UInt32(FENEC_SCHEMA_PLAN)
        let out = try await Fenec.background {
            var text = request
            return try text.withUTF8 { t in
                try Fenec.call { out, len in fenec_schema(handle, t.baseAddress, t.count, mode, out, len) }
            }
        }
        lives.touch(self)
        let v = try Value.parse(bytes: out ?? [])
        let refusals = (v["refusals"]?.array ?? []).map {
            SchemaRefusal(
                kind: $0["kind"]?.string ?? "", collection: $0["collection"]?.string ?? "",
                field: $0["field"]?.string, message: $0["message"]?.string ?? "", fix: $0["fix"]?.string ?? "")
        }
        if !refusals.isEmpty { throw SchemaError(refusals: refusals) }
        return SchemaPlan(
            applied: v["applied"]?.bool ?? false, ran: v["ran"]?.bool ?? false,
            migrations: (v["migrations"]?.array ?? []).compactMap(\.int),
            statements: (v["statements"]?.array ?? []).compactMap(\.string), refusals: [])
    }
}
