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
        ),
        .package(
            url: "https://github.com/k2-fsa/sherpa-onnx",
            exact: "1.13.5"
        ),
    ],
    targets: [
        .target(name: "VoiceSidecarCore"),
        .target(
            name: "VoiceSidecarMacOS",
            dependencies: [
                "VoiceSidecarCore",
                .product(name: "WhisperKit", package: "argmax-oss-swift"),
                .product(name: "sherpa-onnx", package: "sherpa-onnx"),
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
