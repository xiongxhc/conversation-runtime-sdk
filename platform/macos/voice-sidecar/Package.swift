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
    targets: [
        .target(name: "VoiceSidecarCore"),
        .executableTarget(
            name: "ConversationVoiceSidecar",
            dependencies: ["VoiceSidecarCore"]
        ),
        .testTarget(
            name: "VoiceSidecarCoreTests",
            dependencies: ["VoiceSidecarCore"]
        ),
    ]
)
