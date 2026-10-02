import FenecDB
import Foundation
import NotesKit

// notes-cli [add|list|search|done|smoke] -- the app's model from a terminal. No `watch`:
// a file is one process's, so only this process could write what it would
// show; the live query is the app's list, and the smoke runs one.
struct Failure: Error, CustomStringConvertible { let description: String }

func show(_ rows: [Note]) {
    for n in rows {
        let tags = (n.tags ?? []).map { "#\($0)" }.joined(separator: " ")
        print("\(n.id)\t\(n.at.prefix(10))  [\(n.done ? "x" : " ")] \(n.title)  \(tags)")
    }
}

func check(_ ok: Bool, _ step: String) throws {
    guard ok else { throw Failure(description: "smoke: \(step) failed") }
    print("ok  \(step)")
}

/// `op`, or an error after `seconds`.
func within<T: Sendable>(_ seconds: Double, _ op: @escaping @Sendable () async throws -> T) async throws -> T {
    try await withThrowingTaskGroup(of: T.self) { group in
        group.addTask { try await op() }
        group.addTask {
            try await Task.sleep(nanoseconds: UInt64(seconds * 1e9))
            throw Failure(description: "timed out")
        }
        let first = try await group.next()!
        group.cancelAll()
        return first
    }
}

func smoke() async throws {
    let dir = FileManager.default.temporaryDirectory.appendingPathComponent("notes-smoke-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: dir) }
    let file = dir.appendingPathComponent("notes.fenec")

    var notes = try await Notes.open(file)
    let db = notes.db
    try check(try await db.from("notes").count() == 4, "schema and seeds: 4 notes")
    let hello = embed("hello").enumerated().filter { $0.element != 0 }.map(\.offset)
    try check(hello == [24, 36, 46, 48, 62], "toy embedding of \"hello\"")
    try check(try await notes.query().rows(as: Note.self).first?.title == "Book club", "newest first")
    let work = try await notes.query(tag: "work").rows(as: Note.self).map(\.title)
    try check(work == ["Book flights", "Release checklist"], "tag work")
    try check(try await notes.query(openOnly: true).rows().count == 3, "open notes: 3")
    let match = try await db.from("notes").select("title").match("body", "release docs").first()
    try check(match?["title"]?.string == "Release checklist", "match \"release docs\"")
    let near = try await db.from("notes").select("title").near("embed", embed("flights to Istanbul")).first()
    try check(near?["title"]?.string == "Book flights", "near \"flights to Istanbul\"")
    try check(try await notes.query(search: "desert fox").rows(as: Note.self).first?.title == "Book club", "fuse \"desert fox\"")

    // A live query of the open notes: its first rows, then a write.
    let stream = db.live(notes.query(openOnly: true), as: Note.self)
    let added = notes
    let seen = try await within(5) {
        var runs = 0
        for try await rows in stream {
            runs += 1
            if runs == 1 { try await added.add(title: "Call mom", body: "Ask about the weekend.", tags: ["home"]) }
            if rows.count == 4 { return rows.count }
        }
        return 0
    }
    try check(seen == 4, "live query: 4 open notes after an insert")

    let groceries = try await db.from("notes").where("title", "Groceries").first(as: Note.self)!
    try await notes.done(groceries.id)
    try check(try await notes.query(openOnly: true).rows().count == 3, "Groceries done: 3 open")

    try await notes.close()
    notes = try await Notes.open(file)
    try check(try await notes.db.from("notes").count() == 5, "reopened: 5 notes")
    try await notes.close()
}

let args = Array(CommandLine.arguments.dropFirst())
do {
    if args.first == "smoke" {
        try await smoke()
        exit(0)
    }
    let path = ProcessInfo.processInfo.environment["NOTES_FILE"] ?? "notes.fenec"
    let notes = try await Notes.open(URL(fileURLWithPath: path))
    switch args.first ?? "list" {
    case "add" where args.count >= 3:
        try await notes.add(title: args[1], body: args[2], tags: Array(args.dropFirst(3)))
        show(try await notes.query().rows(as: Note.self))
    case "list":
        let tag = args.firstIndex(of: "--tag").flatMap { $0 + 1 < args.count ? args[$0 + 1] : nil } ?? ""
        show(try await notes.query(tag: tag, openOnly: args.contains("--open")).rows(as: Note.self))
    case "search" where args.count >= 2:
        let words = args.dropFirst().joined(separator: " ")
        print("match:")
        show(try await notes.db.from("notes").match("body", words).limit(5).rows(as: Note.self))
        print("fuse (match + near, toy embedding):")
        show(try await notes.query(search: words).limit(5).rows(as: Note.self))
    case "done" where args.count == 2:
        try await notes.done(Int(args[1]) ?? -1)
        show(try await notes.query().rows(as: Note.self))
    default:
        print("usage: notes-cli [add <title> <body> [tag ...] | list [--tag T] [--open] | search <words> | done <id> | smoke]")
        exit(2)
    }
    try await notes.close()
} catch {
    FileHandle.standardError.write("\(error)\n".data(using: .utf8)!)
    exit(1)
}
