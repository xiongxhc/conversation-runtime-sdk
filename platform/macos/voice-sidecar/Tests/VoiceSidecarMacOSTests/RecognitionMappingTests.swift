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

@Test
func malformedLocalTokenizerIsRejectedWithoutNetworkRequests() async throws {
    let folder = FileManager.default.temporaryDirectory.appendingPathComponent(
        UUID().uuidString,
        isDirectory: true
    )
    try FileManager.default.createDirectory(
        at: folder,
        withIntermediateDirectories: true
    )
    defer {
        try? FileManager.default.removeItem(at: folder)
    }
    try Data("{not valid json".utf8).write(
        to: folder.appendingPathComponent("tokenizer.json")
    )
    try Data(
        #"{"tokenizer_class":"PreTrainedTokenizerFast"}"#.utf8
    ).write(
        to: folder.appendingPathComponent("tokenizer_config.json")
    )
    NetworkTrapURLProtocol.reset()
    try #require(
        URLProtocol.registerClass(NetworkTrapURLProtocol.self)
    )
    defer {
        URLProtocol.unregisterClass(NetworkTrapURLProtocol.self)
    }

    await #expect(throws: (any Error).self) {
        _ = try await OfflineWhisperTokenizer.load(from: folder)
    }

    #expect(NetworkTrapURLProtocol.requestCount == 0)
}

@Test
func orderedRecognitionBatchesCannotOvertakeDelayedHandlers() async {
    let gate = AsyncGate()
    let recorder = HypothesisRecorder()
    let pipeline = OrderedRecognitionBatchPipeline(capacity: 8)
    pipeline.start { batch in
        if batch.first?.segmentID == 0 {
            await gate.wait()
        }
        await recorder.append(batch)
    }

    #expect(
        pipeline.enqueue([
            RecognitionHypothesis(segmentID: 0, text: "zero", engineFinal: false)
        ])
    )
    #expect(
        pipeline.enqueue([
            RecognitionHypothesis(segmentID: 1, text: "one", engineFinal: false)
        ])
    )
    #expect(
        pipeline.enqueue([
            RecognitionHypothesis(segmentID: 2, text: "two", engineFinal: true)
        ])
    )
    await gate.open()
    await pipeline.finish()

    #expect(await recorder.segmentIDs == [0, 1, 2])
}

@Test
func publicationRejectionTriggersOneFatalCleanupAndExit() async throws {
    let audio = FatalRecordingAudioService()
    let recognition = FatalRecordingRecognitionService()
    let playback = FatalRecordingPlaybackService()
    let events = FailOnceEventSink()
    let session = SidecarSession(
        audioService: audio,
        recognitionService: recognition,
        playbackService: playback,
        eventSink: events
    )
    try await startFatalSession(session)
    await events.failNext()
    let exitCounter = LockedCounter()
    let controller = SidecarFailureController(
        session: session,
        exitHandler: {
            exitCounter.increment()
        }
    )

    await controller.perform {
        try await session.publishRecognitionHypothesis(
            RecognitionHypothesis(
                segmentID: 1,
                text: "not published",
                engineFinal: false
            )
        )
    }

    #expect(await audio.stopCount == 1)
    #expect(await recognition.stopCount == 1)
    #expect(await playback.stopCount == 1)
    #expect(exitCounter.value == 1)
    #expect(
        await events.frames.last
            == ChildFrame(
                control: .failure(
                    sessionID: 7,
                    stage: .voiceSidecar,
                    code: .internal
                )
            )
    )
}

@Test
func asynchronousOutputFailureUsesTheSameFatalPath() async throws {
    let audio = FatalRecordingAudioService()
    let recognition = FatalRecordingRecognitionService()
    let playback = FatalRecordingPlaybackService()
    let events = FailOnceEventSink()
    let session = SidecarSession(
        audioService: audio,
        recognitionService: recognition,
        playbackService: playback,
        eventSink: events
    )
    try await startFatalSession(session)
    let exitCounter = LockedCounter()
    let controller = SidecarFailureController(
        session: session,
        exitHandler: {
            exitCounter.increment()
        }
    )
    let failure = SidecarServiceFailure(
        stage: .audioOutput,
        code: .playbackFailed
    )

    await controller.terminate(with: failure, fallbackSessionID: 7)

    #expect(await audio.stopCount == 1)
    #expect(await recognition.stopCount == 1)
    #expect(await playback.stopCount == 1)
    #expect(exitCounter.value == 1)
    #expect(
        await events.frames.last
            == ChildFrame(
                control: .failure(
                    sessionID: 7,
                    stage: .audioOutput,
                    code: .playbackFailed
                )
            )
    )
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

private final class NetworkTrapURLProtocol: URLProtocol, @unchecked Sendable {
    private static let lock = NSLock()
    private static var requests = 0

    static var requestCount: Int {
        lock.withLock { requests }
    }

    static func reset() {
        lock.withLock {
            requests = 0
        }
    }

    override class func canInit(with _: URLRequest) -> Bool {
        lock.withLock {
            requests += 1
        }
        return true
    }

    override class func canonicalRequest(for request: URLRequest) -> URLRequest {
        request
    }

    override func startLoading() {
        client?.urlProtocol(
            self,
            didFailWithError: URLError(.networkConnectionLost)
        )
    }

    override func stopLoading() {}
}

private actor AsyncGate {
    private var isOpen = false
    private var waiters: [CheckedContinuation<Void, Never>] = []

    func wait() async {
        if isOpen {
            return
        }
        await withCheckedContinuation { continuation in
            waiters.append(continuation)
        }
    }

    func open() {
        isOpen = true
        let values = waiters
        waiters.removeAll()
        for waiter in values {
            waiter.resume()
        }
    }
}

private actor HypothesisRecorder {
    private(set) var segmentIDs: [UInt64] = []

    func append(_ batch: [RecognitionHypothesis]) {
        segmentIDs.append(contentsOf: batch.map(\.segmentID))
    }
}

private enum FatalTestError: Error {
    case rejectedPublication
}

private actor FailOnceEventSink: SidecarEventSink {
    private(set) var frames: [ChildFrame] = []
    private var shouldFailNext = false

    func failNext() {
        shouldFailNext = true
    }

    func send(_ frame: ChildFrame) throws {
        if shouldFailNext {
            shouldFailNext = false
            throw FatalTestError.rejectedPublication
        }
        frames.append(frame)
    }
}

private actor FatalRecordingAudioService: SidecarAudioService {
    private(set) var stopCount = 0

    func start(configuration _: SidecarConfiguration) {}

    func stop() {
        stopCount += 1
    }
}

private actor FatalRecordingRecognitionService: SidecarRecognitionService {
    private(set) var stopCount = 0

    func start(configuration _: SidecarConfiguration) {}

    func stop() {
        stopCount += 1
    }
}

private actor FatalRecordingPlaybackService: SidecarPlaybackService {
    private(set) var stopCount = 0

    func enqueue(_: PCMFrame) {}

    func flush(throughGenerationID _: UInt64) {}

    func stop() {
        stopCount += 1
    }
}

private final class LockedCounter: @unchecked Sendable {
    private let lock = NSLock()
    private var count = 0

    var value: Int {
        lock.withLock { count }
    }

    func increment() {
        lock.withLock {
            count += 1
        }
    }
}

private func startFatalSession(_ session: SidecarSession) async throws {
    try await session.handleControl(
        ChildFrame(
            control: .startSession(
                sessionID: 7,
                speechStartMilliseconds: 200,
                finalSilenceMilliseconds: 600
            )
        )
    )
    try await session.handleControl(
        ChildFrame(control: .startCapture(sessionID: 7))
    )
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
