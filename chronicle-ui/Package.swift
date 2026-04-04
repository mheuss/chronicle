// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "ChronicleUI",
    platforms: [
        .macOS(.v14),
    ],
    dependencies: [
        .package(url: "https://github.com/swiftlang/swift-testing.git", from: "0.12.0"),
    ],
    targets: [
        .executableTarget(
            name: "ChronicleUI",
            path: "Sources/ChronicleUI"
        ),
        .testTarget(
            name: "ChronicleUITests",
            dependencies: [
                "ChronicleUI",
                .product(name: "Testing", package: "swift-testing"),
            ],
            path: "Tests/ChronicleUITests"
        ),
    ]
)
