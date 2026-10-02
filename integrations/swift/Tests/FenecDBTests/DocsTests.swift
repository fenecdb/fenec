import Testing

@testable import FenecDB

/// The Swift tab of site/content/docs/languages.html, as it is written
/// there, and the builder lines below it.
@Test func theDocsExample() async throws {
    let path = try scratch("docs")
    let db = try await Fenec.open(path: path)

    try await db.execute("create collection if not exists docs (title text, embed vector<3> @hnsw(cosine))")
    try await db.execute("put docs {title: $1, embed: $2}", "Night at the oasis", [Float(0.1), 0.2, 0.3])
    try await db.execute("put docs {title: $1, embed: $2}", "Dunes", [Float(0.9), 0.1, 0.0])

    let rows = try await db.query("get docs select title near embed $1 limit 5", [Float(0.1), 0.2, 0.3])
    #expect(rows[0]["title"] == "Night at the oasis")
    #expect(rows[0]["_score"]?.double != nil)

    do {
        _ = try await db.query("get nowhere")
        Issue.record("a collection that is not there answered")
    } catch let e as FenecError {
        #expect(e.code == .notFound)
    }

    let docs = try db.from("docs")
    try await docs.insert(["title": "Night at the oasis", "embed": .floats([0.1, 0.2, 0.3])] as Value)
    let near = try await docs.select("title").near("embed", [Float(0.1), 0.2, 0.3]).limit(5).rows()
    let n = try await docs.where("title", "~", "Dunes").count()
    #expect(near.count == 3)
    #expect(n == 1)
    // The builder table's condition.
    #expect(
        try docs.where(.or(["lang": "tr"], ["tags": ["has": "rust"]])).toFenecQL().text
            == "get docs where lang = $1 or tags has $2")
    try await db.close()
}
