// The Swift tab of site/content/docs/mobile.html's todo list, compiled with
// the tests so the page cannot drift from the API: its view and its open,
// the `@main` app around them left out (a test bundle has its own entry).
#if canImport(SwiftUI)
    import FenecDB
    import Foundation
    import SwiftUI

    struct DocsTodo: Codable, Sendable, FenecValue {
        var id: Int?
        var title: String
        var done: Bool
    }

    func docsOpen() async throws -> Fenec {
        let dir = try FileManager.default.url(
            for: .applicationSupportDirectory, in: .userDomainMask,
            appropriateFor: nil, create: true)
        let db = try await Fenec.open(dir.appendingPathComponent("todos.fenec"))
        try await db.execute("create collection if not exists todos (title text, done bool @hash)")
        return db
    }

    @available(macOS 12, iOS 15, *)
    struct DocsTodoList: View {
        let db: Fenec
        @StateObject var open: LiveQuery<DocsTodo>

        init(db: Fenec) {
            self.db = db
            _open = StateObject(
                wrappedValue: LiveQuery(db, try! db.from("todos").where("done", false), as: DocsTodo.self))
        }

        var body: some View {
            List(open.rows, id: \.id) { todo in
                Button(todo.title) {
                    Task { try await db.from("todos").where("id", todo.id!).update(["done": true] as Value) }
                }
            }
            .toolbar {
                Button("Add") { Task { try await db.from("todos").insert(DocsTodo(title: "milk", done: false)) } }
            }
        }
    }
#endif
