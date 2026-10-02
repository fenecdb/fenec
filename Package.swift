// swift-tools-version:5.9
//
// FenecDB for Swift: fenecdb embedded in a macOS or iOS app, a file on the
// device. The package lives in integrations/swift; this manifest is at the
// repository's root because SwiftPM fetches a package by its repository
// and tag -- `.package(url: "https://github.com/fenecdb/fenec", from: "X.Y.Z")`.
//
// The engine is the native library (crates/fenec-ffi) as an XCFramework.
// Built here by integrations/swift/build-xcframework.sh, it is taken from
// integrations/swift/build/; otherwise from the release the tag names, by
// its URL and checksum, which the release workflow writes below
// (integrations/swift/set-binary.sh) before the tag is made.
import Foundation
import PackageDescription

let local = "integrations/swift/build/FenecFFI.xcframework"
let here = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
let built = FileManager.default.fileExists(atPath: here.appendingPathComponent(local).path)

let release = "0.1.7"
let checksum = "0000000000000000000000000000000000000000000000000000000000000000"

let ffi: Target =
    built
    ? .binaryTarget(name: "FenecFFI", path: local)
    : .binaryTarget(
        name: "FenecFFI",
        url: "https://github.com/fenecdb/fenec/releases/download/v\(release)/FenecFFI.xcframework.zip",
        checksum: checksum)

let package = Package(
    name: "FenecDB",
    platforms: [.macOS(.v12), .iOS(.v15)],
    products: [
        .library(name: "FenecDB", targets: ["FenecDB"])
    ],
    targets: [
        ffi,
        .target(
            name: "FenecDB",
            dependencies: ["FenecFFI"],
            path: "integrations/swift/Sources/FenecDB"),
        .testTarget(
            name: "FenecDBTests",
            dependencies: ["FenecDB"],
            path: "integrations/swift/Tests/FenecDBTests"),
    ]
)
