import Darwin
import Foundation
import VoiceSidecarCore
import VoiceSidecarMacOS

private struct LaunchConfiguration {
    let modelPath: String

    init(arguments: [String]) throws {
        var modelPath: String?
        var device: String?
        var download: String?
        var index = 0
        while index < arguments.count {
            guard index + 1 < arguments.count else {
                throw LaunchConfigurationError.invalidArguments
            }
            switch arguments[index] {
            case "--model-path":
                modelPath = arguments[index + 1]
            case "--device":
                device = arguments[index + 1]
            case "--download":
                download = arguments[index + 1]
            default:
                throw LaunchConfigurationError.invalidArguments
            }
            index += 2
        }
        var modelIsDirectory = ObjCBool(false)
        guard let modelPath,
            (modelPath as NSString).isAbsolutePath,
            FileManager.default.fileExists(
                atPath: modelPath,
                isDirectory: &modelIsDirectory
            ),
            modelIsDirectory.boolValue,
            device == "system-default",
            download == "false"
        else {
            throw LaunchConfigurationError.invalidArguments
        }
        self.modelPath = modelPath
    }
}

private enum LaunchConfigurationError: Error {
    case invalidArguments
}

private let launchConfiguration: LaunchConfiguration
do {
    launchConfiguration = try LaunchConfiguration(
        arguments: Array(CommandLine.arguments.dropFirst())
    )
} catch {
    try? FileHandle.standardError.write(
        contentsOf: Data("voice sidecar configuration failed\n".utf8)
    )
    exit(EXIT_FAILURE)
}

let eventWriter = SerializedFrameWriter(
    writer: FileHandleFrameWriter(fileHandle: .standardOutput)
)
let engine = VoiceProcessingEngine()
let audioProcessor = VoiceProcessingAudioProcessor(engine: engine)
let recognition = WhisperKitRecognition(
    modelPath: launchConfiguration.modelPath,
    audioProcessor: audioProcessor
)
let playback = ContinuousPCMPlayback(scheduler: engine)
let session = SidecarSession(
    audioService: engine,
    recognitionService: recognition,
    playbackService: playback,
    eventSink: eventWriter
)
let failureController = SidecarFailureController(
    session: session,
    exitHandler: {
        exit(EXIT_FAILURE)
    }
)
await playback.setRenderedHandler { identity in
    await failureController.perform {
        try await session.playbackRendered(identity)
    }
}
await playback.setFailureHandler { failure in
    await failureController.terminate(
        with: failure,
        fallbackSessionID: 0
    )
}
await recognition.setEventHandler { event in
    switch event {
    case .hypothesis(let hypothesis):
        try await session.publishRecognitionHypothesis(hypothesis)
        return false
    case .voiceWindow(
        let
            isSpeech,
        let
            frameMilliseconds,
        let
            atMilliseconds
    ):
        return try await session.observeBargeIn(
            isSpeech: isSpeech,
            frameMilliseconds: frameMilliseconds,
            atMilliseconds: atMilliseconds
        )
    case .activity(let activity):
        try await session.publishVoiceActivity(activity)
        return false
    case .failure(let sessionID, let failure):
        await failureController.terminate(
            with: failure,
            fallbackSessionID: sessionID
        )
        return false
    }
}
await recognition.setFailureHandler { sessionID, failure in
    await failureController.terminate(
        with: failure,
        fallbackSessionID: sessionID
    )
}
engine.setFailureHandler { sessionID, failure in
    await failureController.terminate(
        with: failure,
        fallbackSessionID: sessionID
    )
}
let stdio = FramedStdio(
    controlReader: FileHandleFrameReader(fileHandle: .standardInput),
    mediaReader: FileHandleFrameReader(
        fileHandle: FileHandle(fileDescriptor: 3, closeOnDealloc: false)
    )
)

do {
    try await stdio.run(
        onControl: { frame in
            try await session.handleControl(frame)
            return await session.isTerminated ? .stop : .continue
        },
        onMedia: { frame in
            try await session.handleMedia(frame)
            return .continue
        }
    )
} catch {
    await failureController.terminate(
        with: error,
        fallbackSessionID: 0
    )
}
