import Foundation
import Testing

@testable import VoiceSidecarCore
@testable import VoiceSidecarMacOS

@Test
func captureTimeoutCancelsWaiterAndAlwaysRunsCleanup() async {
    let waiter = CaptureBufferWaiter()
    let cleanup = CleanupRecorder()

    await #expect(throws: HardwareAcceptanceError.captureTimedOut) {
        try await withGuaranteedAsyncCleanup(
            operation: {
                try await waiter.wait(timeout: .milliseconds(1))
            },
            cleanup: {
                await cleanup.run()
            }
        )
    }

    waiter.signal()
    #expect(await cleanup.count == 1)
}

@Test
func optInFullDuplexHardwareSmoke() async throws {
    guard
        ProcessInfo.processInfo.environment[
            "CONVERSATION_RUN_HARDWARE_ACCEPTANCE"
        ] == "1"
    else {
        return
    }
    let modelPath = try #require(
        ProcessInfo.processInfo.environment[
            "CONVERSATION_WHISPERKIT_MODEL_PATH"
        ]
    )
    let modelURL = URL(fileURLWithPath: modelPath, isDirectory: true)
    var modelIsDirectory = ObjCBool(false)
    try #require((modelPath as NSString).isAbsolutePath)
    try #require(
        FileManager.default.fileExists(
            atPath: modelURL.path,
            isDirectory: &modelIsDirectory
        )
    )
    try #require(modelIsDirectory.boolValue)

    let engine = VoiceProcessingEngine()
    let processor = VoiceProcessingAudioProcessor(engine: engine)
    let playback = ContinuousPCMPlayback(scheduler: engine)
    let captured = CaptureBufferWaiter()
    let rendered = PlaybackRenderWaiter()
    try await withGuaranteedAsyncCleanup(
        operation: {
            await playback.setRenderedHandler { identity in
                rendered.signal(identity)
            }
            try processor.startRecordingLive { samples in
                guard !samples.isEmpty else {
                    return
                }
                captured.signal()
            }
            try await engine.start(
                configuration: SidecarConfiguration(
                    sessionID: 1,
                    speechStartMilliseconds: 200,
                    finalSilenceMilliseconds: 600
                )
            )
            let devices = try await engine.activeAudioDeviceStatus()
            #expect(!devices.inputLabel.isEmpty)
            #expect(!devices.outputLabel.isEmpty)
            try await captured.wait(timeout: .seconds(10))
            #expect(engine.isRunning)

            let frame = try PCMFrame(
                turnID: 1,
                generationID: 1,
                utteranceID: 1,
                sequence: 0,
                format: PCMFormat(
                    sampleRateHz: 16_000,
                    channels: 1,
                    sampleFormat: .float32LittleEndian
                ),
                bytes: Data(
                    count: 1_600 * MemoryLayout<Float>.size
                )
            )
            try await playback.enqueue(frame)
            let renderedIdentity = try await rendered.wait(
                timeout: .seconds(5)
            )
            #expect(renderedIdentity == frame.identity)
            try await playback.flush(throughGenerationID: 1)
        },
        cleanup: {
            await playback.stop()
            processor.stopRecording()
            await engine.stop()
        }
    )

    #expect(!engine.isRunning)
}

final class PlaybackRenderWaiter: @unchecked Sendable {
    private let stream: AsyncStream<PlaybackFrameIdentity>
    private let continuation: AsyncStream<PlaybackFrameIdentity>.Continuation

    init() {
        (stream, continuation) = AsyncStream.makeStream(
            bufferingPolicy: .bufferingNewest(1)
        )
    }

    func signal(_ identity: PlaybackFrameIdentity) {
        continuation.yield(identity)
    }

    func wait(timeout: Duration) async throws -> PlaybackFrameIdentity {
        try await withThrowingTaskGroup(
            of: PlaybackFrameIdentity.self
        ) { group in
            group.addTask { [stream] in
                for await identity in stream {
                    return identity
                }
                throw CancellationError()
            }
            group.addTask {
                try await Task.sleep(for: timeout)
                throw HardwareAcceptanceError.playbackTimedOut
            }
            defer {
                group.cancelAll()
            }
            let identity = try await group.next()!
            group.cancelAll()
            return identity
        }
    }

    deinit {
        continuation.finish()
    }
}

func withGuaranteedAsyncCleanup<Result: Sendable>(
    operation: () async throws -> Result,
    cleanup: () async -> Void
) async throws -> Result {
    do {
        let result = try await operation()
        await cleanup()
        return result
    } catch {
        await cleanup()
        throw error
    }
}

final class CaptureBufferWaiter: @unchecked Sendable {
    private let stream: AsyncStream<Void>
    private let continuation: AsyncStream<Void>.Continuation

    init() {
        (stream, continuation) = AsyncStream.makeStream(
            bufferingPolicy: .bufferingNewest(1)
        )
    }

    func signal() {
        continuation.yield()
    }

    func wait(timeout: Duration) async throws {
        try await withThrowingTaskGroup(of: Void.self) { group in
            group.addTask { [stream] in
                for await _ in stream {
                    return
                }
                throw CancellationError()
            }
            group.addTask {
                try await Task.sleep(for: timeout)
                throw HardwareAcceptanceError.captureTimedOut
            }
            defer {
                group.cancelAll()
            }
            _ = try await group.next()!
            group.cancelAll()
        }
    }

    deinit {
        continuation.finish()
    }
}

private enum HardwareAcceptanceError: Error {
    case captureTimedOut
    case playbackTimedOut
}

private actor CleanupRecorder {
    private(set) var count = 0

    func run() {
        count += 1
    }
}
