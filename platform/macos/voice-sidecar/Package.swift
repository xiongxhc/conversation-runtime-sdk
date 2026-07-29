// swift-tools-version: 5.10
import PackageDescription

let package = Package(
    name: "conversation-voice-sidecar",
    platforms: [.macOS(.v14)],
    products: [
        .executable(
            name: "conversation-voice-sidecar",
            targets: ["ConversationVoiceSidecar"]
        ),
    ],
    dependencies: [
        .package(
            url: "https://github.com/argmaxinc/argmax-oss-swift.git",
            exact: "1.0.0"
        )
    ],
    targets: [
        .target(name: "VoiceSidecarCore"),
        .target(
            name: "VoiceSidecarMacOS",
            dependencies: [
                "VoiceSidecarCore",
                .product(name: "WhisperKit", package: "argmax-oss-swift"),
            ]
        ),
        .executableTarget(
            name: "ConversationVoiceSidecar",
            dependencies: ["VoiceSidecarCore", "VoiceSidecarMacOS"]
        ),
        .testTarget(
            name: "VoiceSidecarCoreTests",
            dependencies: ["VoiceSidecarCore"]
        ),
        .testTarget(
            name: "VoiceSidecarMacOSTests",
            dependencies: ["VoiceSidecarCore", "VoiceSidecarMacOS"]
        ),
    ]
)
