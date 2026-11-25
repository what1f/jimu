// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "JimuKit",
    platforms: [
        .macOS(.v15),
        .iOS(.v17),
    ],
    products: [
        .library(
            name: "JimuKit",
            targets: ["JimuKit"]
        ),
    ],
    targets: [
        .binaryTarget(
            name: "CJimu",
            path: "Artifacts/JimuCore.xcframework"
        ),
        .target(
            name: "JimuKit",
            dependencies: ["CJimu"],
            swiftSettings: [
                .swiftLanguageMode(.v6),
            ]
        ),
        .testTarget(
            name: "JimuKitTests",
            dependencies: ["JimuKit"],
            swiftSettings: [
                .swiftLanguageMode(.v6),
            ]
        ),
    ]
)
