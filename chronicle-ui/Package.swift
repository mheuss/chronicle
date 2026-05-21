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
            path: "Sources/ChronicleUI",
            exclude: ["Info.plist"],
            linkerSettings: [
                .unsafeFlags([
                    "-Xlinker", "-sectcreate",
                    "-Xlinker", "__TEXT",
                    "-Xlinker", "__info_plist",
                    "-Xlinker", "Sources/ChronicleUI/Info.plist",
                ]),
            ]
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
