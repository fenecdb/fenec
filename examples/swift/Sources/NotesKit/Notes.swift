import FenecDB
import Foundation

public struct Note: Codable, Sendable, Identifiable, Hashable {
    public var id: Int
    public var title: String
    public var body: String
    public var tags: [String]?
    public var done: Bool
    public var at: String
}

/// The notes in a fenecdb file: the schema, the seeds, the queries.
public final class Notes: Sendable {
    public let db: Fenec

    init(db: Fenec) { self.db = db }

    /// Opens (or makes) the file, brings its schema up to schema.fenecql and
    /// seeds an empty collection.
    public static func open(_ url: URL) async throws -> Notes {
        let db = try await Fenec.open(url)
        let schema = try String(contentsOf: Bundle.module.url(forResource: "schema", withExtension: "fenecql")!)
        try await db.schema(schema)
        let notes = Notes(db: db)
        if try await db.from("notes").count() == 0 {
            for s in seeds { try await notes.add(title: s.0, body: s.1, tags: s.2, done: s.3, at: s.4) }
        }
        return notes
    }

    /// Application Support/Notes/notes.fenec: backed up, never shown in Files.
    public static func defaultURL() throws -> URL {
        let dir = try FileManager.default
            .url(for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true)
            .appendingPathComponent("Notes")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent("notes.fenec")
    }

    public func add(title: String, body: String, tags: [String], done: Bool = false, at: String? = nil) async throws {
        try await db.from("notes").insert([
            "title": .string(title), "body": .string(body), "tags": .array(tags.map { .string($0) }),
            "done": .bool(done), "at": try at.map(Value.string) ?? Date().fenecValue(),
            "embed": .floats(embed("\(title) \(body)")),
        ] as Row)
    }

    public func done(_ id: Int) async throws {
        try await db.from("notes").where("id", id).update(["done": true] as Row)
    }

    /// The list on screen: newest first, or ranked by `words` -- the words
    /// (match) and the toy embedding (near) fused -- with the filters.
    /// Only names are checked by the builder, and these are constants.
    public func query(search words: String = "", tag: String = "", openOnly: Bool = false) -> Query {
        var q = try! db.from("notes").select("id", "title", "body", "tags", "done", "at")
        if !tag.isEmpty { q = try! q.where("tags", "has", tag) }
        if openOnly { q = try! q.where("done", false) }
        if words.isEmpty { return try! q.order("at", "desc").limit(20) }
        return try! q.match("body", words).near("embed", embed(words)).fuse().limit(20)
    }

    public func close() async throws { try await db.close() }
}

let seeds: [(String, String, [String], Bool, String)] = [
    ("Groceries", "Buy milk, eggs and fresh bread for the weekend.", ["home", "shopping"], false, "2026-09-28T09:00:00Z"),
    ("Release checklist", "Tag the release, publish the packages and update the docs.", ["work"], false, "2026-09-29T09:00:00Z"),
    ("Book flights", "Find cheap flights to Istanbul for the conference in spring.", ["travel", "work"], true, "2026-09-30T09:00:00Z"),
    ("Book club", "Finish the novel about the desert fox before Thursday.", ["home", "reading"], false, "2026-10-01T09:00:00Z"),
]

/// A TOY embedding, a placeholder for a real model: character trigrams
/// (of the UTF-8 bytes) hashed into 64 dimensions, so `near` and `fuse`
/// have something honest to rank. A real app computes the vector with a
/// model -- Core ML on the device, or an embeddings API -- and declares
/// `vector<N>` with that model's N.
public func embed(_ text: String) -> [Float] {
    let bytes: [UInt8] = [32] + text.utf8.map { (65...90).contains($0) ? $0 + 32 : $0 } + [32]
    var v = [Float](repeating: 0, count: 64)
    for i in 0..<(bytes.count - 2) {
        var h: UInt32 = 0x811c_9dc5
        for b in bytes[i..<i + 3] { h = (h ^ UInt32(b)) &* 0x0100_0193 }
        v[Int(h % 64)] += 1
    }
    let norm = v.reduce(0) { $0 + $1 * $1 }.squareRoot()
    return norm > 0 ? v.map { $0 / norm } : v
}
