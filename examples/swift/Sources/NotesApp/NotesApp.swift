import AppKit
import NotesKit
import SwiftUI

@main struct NotesApp: App {
    @State private var notes: Notes?
    @State private var failure: String?

    init() {
        // Run from `swift run`, outside an app bundle: still a window in front.
        NSApplication.shared.setActivationPolicy(.regular)
    }

    var body: some Scene {
        WindowGroup {
            if let notes {
                NotesView(notes: notes).frame(minWidth: 640, minHeight: 480)
            } else if let failure {
                Text(failure)
            } else {
                ProgressView().task {
                    do { notes = try await Notes.open(Notes.defaultURL()) } catch { failure = "\(error)" }
                }
            }
        }
    }
}
