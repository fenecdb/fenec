import Testing

@testable import FenecDB

/// A schema written as FenecQL, through the native library: made, checked
/// again, refused when it would lose data, and migrated once.
@Suite struct Schema {
    @Test func aSchemaIsMadeCheckedAndMigrated() async throws {
        let file = try scratch("schema")
        let v1 = "create collection notes (title text required, at timestamp @sorted)"

        let db = try await open(url: file, options: [])
        let made = try await db.schema(v1)
        #expect(made.applied && made.statements == [v1])
        try await db.execute("put notes {title: \"a\"}")
        try await db.close()

        // Opened again with the same schema: nothing to do.
        let again = try await open(url: file, options: [])
        let nothing = SchemaPlan(applied: false, ran: false, migrations: [], statements: [], refusals: [])
        #expect(try await again.schema(v1) == nothing)
        // A rename unsaid could be a drop and an add: refused, nothing written.
        let v2 = "create collection notes (name text required, at timestamp @sorted)"
        await #expect(throws: SchemaError.self) { try await again.schema(v2) }
        // Said, as a migration: run once, recorded.
        let moved = ["alter collection notes rename field title to name"]
        let migrated = try await again.schema(v2, migrations: moved)
        #expect(migrated.ran && migrated.migrations == [1])
        #expect(try await again.query("get notes select name").map { $0["name"]?.string } == ["a"])
        #expect(try await again.schema(v2, migrations: moved).migrations.isEmpty)
        try await again.close()
    }
}
