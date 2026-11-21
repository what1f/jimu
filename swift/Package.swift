// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "PikraKit",
    platforms: [
        .macOS(.v15),
        .iOS(.v17),
    ],
    products: [
        .library(
            name: "PikraKit",
            targets: ["PikraKit"]
        ),
    ],
    targets: [
        .binaryTarget(
            name: "CPikra",
            path: "Artifacts/PikraCore.xcframework"
        ),
        .target(
            name: "PikraKit",
            dependencies: ["CPikra"],
            swiftSettings: [
                .swiftLanguageMode(.v6),
            ]
        ),
        .testTarget(
            name: "PikraKitTests",
            dependencies: ["PikraKit"],
            swiftSettings: [
                .swiftLanguageMode(.v6),
            ]
        ),
    ]
)
