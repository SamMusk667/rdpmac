// swift-tools-version: 5.10
import PackageDescription

// The menu-bar app. scripts/build-app.sh assembles rdpmac.app from this binary, the daemon and
// the files in Bundle/.
let package = Package(
    name: "rdpmac-app",
    platforms: [.macOS(.v13)],
    targets: [
        .executableTarget(name: "rdpmac", path: "Sources/rdpmac"),
    ]
)
