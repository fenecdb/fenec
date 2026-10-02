// swift-tools-version:5.9
// Notes: a SwiftUI app over fenecdb embedded in it, a file on the device.
// NotesKit holds the model and the views (macOS and iOS alike), NotesApp is
// the macOS app, notes-cli the same model from a terminal.
import Foundation
import PackageDescription

// FENEC_LOCAL=1 builds against this repository's FenecDB (CI does), its
// XCFramework from integrations/swift/build-xcframework.sh; otherwise the
// release. A path dependency is named by its directory.
let local = ProcessInfo.processInfo.environment["FENEC_LOCAL"] != nil
let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent("../..").standardized
let fenec: Package.Dependency =
    local
    ? .package(path: root.path)
    : .package(url: "https://github.com/fenecdb/fenec", from: "0.1.9")
let fenecPackage = local ? root.lastPathComponent : "fenec"

let package = Package(
    name: "Notes",
    platforms: [.macOS(.v12), .iOS(.v15)],
    products: [
        .library(name: "NotesKit", targets: ["NotesKit"]),
        .executable(name: "NotesApp", targets: ["NotesApp"]),
        .executable(name: "notes-cli", targets: ["notes-cli"]),
    ],
    dependencies: [fenec],
    targets: [
        .target(
            name: "NotesKit",
            dependencies: [.product(name: "FenecDB", package: fenecPackage)],
            resources: [.copy("schema.fenecql")]),
        .executableTarget(name: "NotesApp", dependencies: ["NotesKit"]),
        .executableTarget(name: "notes-cli", dependencies: ["NotesKit"]),
    ]
)
