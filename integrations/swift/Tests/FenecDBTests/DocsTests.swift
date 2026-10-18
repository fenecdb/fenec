import Testing

@testable import FenecDB

/// The Swift tab of site/content/docs/languages.html, as it is written
/// there, and the builder lines below it.
@Test func theDocsExample() async throws {
    let path = try scratch("docs")
    let db = try await Fenec.open(path: path)

    try await db.execute("create collection if not exists docs (title text @text, year int @sorted)")
    try await db.execute("put docs {title: $1, year: $2}", "Dunes", 2021)
    try await db.execute("put docs {title: $1, year: $2}", "Night at the oasis", 2024)

    let rows = try await db.query("get docs select title, year where year >= $1 order year desc limit $2", 2020, 5)
    #expect(rows.compactMap { $0["title"]?.string } == ["Night at the oasis", "Dunes"])
    #expect(rows[0]["year"] == 2024)
    let hits = try await db.query("get docs select title match title $1 fuzzy", "oasys")
    #expect(hits.compactMap { $0["title"]?.string } == ["Night at the oasis"])
    #expect((hits[0]["_score"]?.double ?? 0) > 0)

    do {
        _ = try await db.query("get nowhere")
        Issue.record("a collection that is not there answered")
    } catch let e as FenecError {
        #expect(e.code == .notFound)
    }

    let docs = try db.from("docs")
    try await docs.insert(["title": "Night at the oasis", "year": 2024] as Value)
    let recent = try await docs.select("title", "year").where("year", ">=", 2020).order("year", "desc").limit(5).rows()
    let found = try await docs.select("title").match("title", "oasys", fuzzy: 2).rows()
    let n = try await docs.where("title", "~", "Dunes").count()
    #expect(recent.compactMap { $0["title"]?.string } == ["Night at the oasis", "Night at the oasis", "Dunes"])
    #expect(found.count == 2)
    #expect(n == 1)
    // The builder table's condition.
    #expect(
        try docs.where(.or(["lang": "tr"], ["tags": ["has": "rust"]])).toFenecQL().text
            == "get docs where lang = $1 or tags has $2")
    try await db.close()
}
