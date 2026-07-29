import AVFoundation
import Foundation
import Testing

@testable import VoiceSidecarCore
@testable import VoiceSidecarMacOS

@Test
func changedCurrentTextReplacesThePartialForTheSameSegment() {
    let mapper = RecognitionMapper()
    let oldState = RecognitionStateSnapshot(currentText: "hel")
    let newState = RecognitionStateSnapshot(currentText: "hello")

    #expect(
        mapper.changes(from: oldState, to: newState)
            == [
                RecognitionHypothesis(
                    segmentID: 0,
                    text: "hello",
                    engineFinal: false
                )
            ]
    )
}

@Test
func unconfirmedSegmentsProduceOnePartialHypothesis() {
    let mapper = RecognitionMapper()
    let oldState = RecognitionStateSnapshot()
    let newState = RecognitionStateSnapshot(
        unconfirmedSegments: [
            RecognitionSegmentSnapshot(id: 4, text: "hello "),
            RecognitionSegmentSnapshot(id: 5, text: "world"),
        ]
    )

    #expect(
        mapper.changes(from: oldState, to: newState)
            == [
                RecognitionHypothesis(
                    segmentID: 1,
                    text: "hello world",
                    engineFinal: false
                )
            ]
    )
}

@Test
func newlyConfirmedSegmentsMapToEngineFinalWithoutConversationFinalization() {
    let mapper = RecognitionMapper()
    let oldState = RecognitionStateSnapshot(
        confirmedSegments: [
            RecognitionSegmentSnapshot(id: 7, text: "already confirmed")
        ]
    )
    let newState = RecognitionStateSnapshot(
        confirmedSegments: [
            RecognitionSegmentSnapshot(id: 7, text: "already confirmed"),
            RecognitionSegmentSnapshot(id: 8, text: "engine confirmed"),
        ]
    )

    #expect(
        mapper.changes(from: oldState, to: newState)
            == [
                RecognitionHypothesis(
                    segmentID: 1,
                    text: "engine confirmed",
                    engineFinal: true
                )
            ]
    )
}

@Test
func repeatedWhisperKitSegmentIDsStillProduceNewConfirmedHypotheses() {
    let mapper = RecognitionMapper()
    let oldState = RecognitionStateSnapshot(
        confirmedSegments: [
            RecognitionSegmentSnapshot(id: 0, text: "first pass")
        ]
    )
    let newState = RecognitionStateSnapshot(
        confirmedSegments: [
            RecognitionSegmentSnapshot(id: 0, text: "first pass"),
            RecognitionSegmentSnapshot(id: 0, text: "second pass"),
        ]
    )

    #expect(
        mapper.changes(from: oldState, to: newState)
            == [
                RecognitionHypothesis(
                    segmentID: 1,
                    text: "second pass",
                    engineFinal: true
                )
            ]
    )
}

@Test
func unchangedAndWhitespaceRecognitionStateEmitsNothing() {
    let mapper = RecognitionMapper()
    let state = RecognitionStateSnapshot(
        currentText: " ",
        confirmedSegments: [
            RecognitionSegmentSnapshot(id: 1, text: "\n")
        ]
    )

    #expect(mapper.changes(from: state, to: state).isEmpty)
}

@Test
func authorizedMicrophoneDoesNotRequestPermission() async throws {
    let requests = PermissionRequestCounter()
    let provider = StubMicrophonePermissionProvider(
        status: .authorized,
        requestResult: false,
        requests: requests
    )

    try await VoiceProcessingEngine.requireMicrophonePermission(using: provider)

    #expect(await requests.count == 0)
}

@Test(arguments: [MicrophoneAuthorization.denied, .restricted])
func unavailableMicrophoneAuthorizationIsTyped(
    status: MicrophoneAuthorization
) async {
    let provider = StubMicrophonePermissionProvider(
        status: status,
        requestResult: true,
        requests: PermissionRequestCounter()
    )

    await expectPermissionDenied {
        try await VoiceProcessingEngine.requireMicrophonePermission(using: provider)
    }
}

@Test
func notDeterminedMicrophoneAuthorizationAcceptsGrantedRequest() async throws {
    let requests = PermissionRequestCounter()
    let provider = StubMicrophonePermissionProvider(
        status: .notDetermined,
        requestResult: true,
        requests: requests
    )

    try await VoiceProcessingEngine.requireMicrophonePermission(using: provider)

    #expect(await requests.count == 1)
}

@Test
func declinedMicrophoneRequestIsTypedBeforeCaptureStarts() async {
    let requests = PermissionRequestCounter()
    let provider = StubMicrophonePermissionProvider(
        status: .notDetermined,
        requestResult: false,
        requests: requests
    )
    let engine = VoiceProcessingEngine(permissionProvider: provider)

    await expectPermissionDenied {
        try await engine.start(
            configuration: SidecarConfiguration(
                sessionID: 1,
                speechStartMilliseconds: 200,
                finalSilenceMilliseconds: 600
            )
        )
    }
    #expect(await requests.count == 1)
    #expect(!engine.isRunning)
}

@Test
func flushSuppressesLateRenderedCallbacksFromTheOldEpoch() async throws {
    let scheduler = RecordingPlaybackScheduler()
    let rendered = RenderedRecorder()
    let playback = ContinuousPCMPlayback(scheduler: scheduler)
    await playback.setRenderedHandler { identity in
        await rendered.append(identity)
    }
    let frame = try pcmFrame(generationID: 9)

    try await playback.enqueue(frame)
    try await playback.flush(throughGenerationID: 9)
    scheduler.completeScheduledBuffer(at: 0)
    await Task.yield()

    #expect(await rendered.identities.isEmpty)
    #expect(scheduler.resetCount == 1)
}

private actor PermissionRequestCounter {
    private(set) var count = 0

    func increment() {
        count += 1
    }
}

private struct StubMicrophonePermissionProvider: MicrophonePermissionProviding {
    let status: MicrophoneAuthorization
    let requestResult: Bool
    let requests: PermissionRequestCounter

    func authorizationStatus() -> MicrophoneAuthorization {
        status
    }

    func requestAccess() async -> Bool {
        await requests.increment()
        return requestResult
    }
}

private func expectPermissionDenied(
    _ operation: () async throws -> Void
) async {
    do {
        try await operation()
        Issue.record("expected microphone permission failure")
    } catch let error as SidecarServiceFailure {
        #expect(
            error
                == SidecarServiceFailure(
                    stage: .audioCapture,
                    code: .permissionDenied
                )
        )
    } catch {
        Issue.record("unexpected error \(error)")
    }
}

private final class RecordingPlaybackScheduler: PCMPlaybackScheduling, @unchecked Sendable {
    let playbackFormat = AVAudioFormat(
        commonFormat: .pcmFormatFloat32,
        sampleRate: 16_000,
        channels: 1,
        interleaved: false
    )!
    private let lock = NSLock()
    private var completions: [@Sendable () -> Void] = []
    private(set) var resetCount = 0

    func schedulePlayback(
        _: AVAudioPCMBuffer,
        completion: @escaping @Sendable () -> Void
    ) throws {
        lock.withLock {
            completions.append(completion)
        }
    }

    func resetPlayback() {
        lock.withLock {
            resetCount += 1
        }
    }

    func completeScheduledBuffer(at index: Int) {
        let completion = lock.withLock {
            completions[index]
        }
        completion()
    }
}

private actor RenderedRecorder {
    private(set) var identities: [PlaybackFrameIdentity] = []

    func append(_ identity: PlaybackFrameIdentity) {
        identities.append(identity)
    }
}

private func pcmFrame(generationID: UInt64) throws -> PCMFrame {
    let sample: Float = 0
    return try PCMFrame(
        turnID: generationID,
        generationID: generationID,
        utteranceID: 1,
        sequence: 0,
        format: PCMFormat(
            sampleRateHz: 16_000,
            channels: 1,
            sampleFormat: .float32LittleEndian
        ),
        bytes: withUnsafeBytes(of: sample) { Data($0) }
    )
}
