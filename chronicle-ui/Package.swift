// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "ChronicleUI",
    platforms: [
        .macOS(.v14),
    ],
    targets: [
        .executableTarget(
            name: "ChronicleUI",
            path: "Sources/ChronicleUI"
        ),
    ]
)
