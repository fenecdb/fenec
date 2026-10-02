import FenecDB
import SwiftUI

/// One screen: an add form, a search box and filters over a live list.
public struct NotesView: View {
    let notes: Notes
    @State private var search = ""
    @State private var searched = ""
    @State private var tag = ""
    @State private var openOnly = false
    @State private var title = ""
    @State private var body_ = ""
    @State private var tags = ""

    public init(notes: Notes) { self.notes = notes }

    public var body: some View {
        VStack(alignment: .leading) {
            HStack {
                TextField("Title", text: $title)
                TextField("Note", text: $body_)
                TextField("tags, comma separated", text: $tags)
                Button("Add") {
                    let tagList = tags.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }
                    let (t, b) = (title, body_)
                    Task { try await notes.add(title: t, body: b, tags: tagList) }
                    (title, body_, tags) = ("", "", "")
                }
                .disabled(title.isEmpty)
            }
            HStack {
                TextField("Search", text: $search).onSubmit { searched = search }
                TextField("Tag", text: $tag)
                Toggle("Open only", isOn: $openOnly)
            }
            // A new filter is a new live query.
            NoteList(notes: notes, query: notes.query(search: searched, tag: tag, openOnly: openOnly))
                .id("\(searched)|\(tag)|\(openOnly)")
        }
        .padding()
    }
}

/// The rows of a live query: drawn again after every write to `notes`.
struct NoteList: View {
    let notes: Notes
    @StateObject private var live: LiveQuery<Note>

    init(notes: Notes, query: Query) {
        self.notes = notes
        _live = StateObject(wrappedValue: LiveQuery(notes.db, query, as: Note.self))
    }

    var body: some View {
        List(live.rows) { note in
            VStack(alignment: .leading, spacing: 2) {
                Text(note.title).font(.headline).strikethrough(note.done)
                Text(note.body).font(.body)
                Text("\(note.at.prefix(10))  \((note.tags ?? []).map { "#\($0)" }.joined(separator: " "))")
                    .font(.caption).foregroundColor(.secondary)
            }
            .contentShape(Rectangle())
            .onTapGesture { Task { try await notes.done(note.id) } }
        }
    }
}
