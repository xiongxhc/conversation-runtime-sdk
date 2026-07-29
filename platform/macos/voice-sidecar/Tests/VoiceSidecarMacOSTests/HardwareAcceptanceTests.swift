import Foundation
import Testing

@testable import VoiceSidecarCore
@testable import VoiceSidecarMacOS

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
    #expect((modelPath as NSString).isAbsolutePath)
    #expect(
        FileManager.default.fileExists(
            atPath: modelURL.path,
            isDirectory: &modelIsDirectory
        )
    )
    #expect(modelIsDirectory.boolValue)

    let engine = VoiceProcessingEngine()
    let processor = VoiceProcessingAudioProcessor(engine: engine)
    let captured = CapturedBufferLatch()
    try processor.startRecordingLive { samples in
        guard !samples.isEmpty else {
            return
        }
        Task {
            await captured.signal()
        }
    }

    try await engine.start(
        configuration: SidecarConfiguration(
            sessionID: 1,
            speechStartMilliseconds: 200,
            finalSilenceMilliseconds: 600
        )
    )
    try await captured.wait()
    #expect(engine.isRunning)

    let playback = ContinuousPCMPlayback(scheduler: engine)
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
        bytes: Data(count: 1_600 * MemoryLayout<Float>.size)
    )
    try await playback.enqueue(frame)
    try await playback.flush(throughGenerationID: 1)
    await playback.stop()
    processor.stopRecording()
    await engine.stop()

    #expect(!engine.isRunning)
}

private actor CapturedBufferLatch {
    private var signalled = false
    private var continuation: CheckedContinuation<Void, Never>?

    func signal() {
        signalled = true
        continuation?.resume()
        continuation = nil
    }

    func wait() async throws {
        if signalled {
            return
        }
        try await withThrowingTaskGroup(of: Void.self) { group in
            group.addTask {
                await self.waitForSignal()
            }
            group.addTask {
                try await Task.sleep(for: .seconds(10))
                throw HardwareAcceptanceError.captureTimedOut
            }
            _ = try await group.next()
            group.cancelAll()
        }
    }

    private func waitForSignal() async {
        if signalled {
            return
        }
        await withCheckedContinuation { value in
            store(value)
        }
    }

    private func store(_ value: CheckedContinuation<Void, Never>) {
        if signalled {
            value.resume()
        } else {
            continuation = value
        }
    }
}

private enum HardwareAcceptanceError: Error {
    case captureTimedOut
}
