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
func incompleteMultibyteTokenPrefixUsesOnlyCompletedDecodeIndices() {
    let result = UnicodeTokenSplitter.split(tokens: [1, 2]) { tokens in
        switch tokens {
        case [1]:
            "\u{fffd}"
        case [1, 2]:
            "é"
        default:
            ""
        }
    }

    #expect(result.words == ["é"])
    #expect(result.wordTokens == [[1, 2]])
}

@Test
func captureDiscontinuityResetsRecognitionSpeechAccumulation() {
    var gate = RecognitionSpeechGate(thresholdMilliseconds: 300)

    #expect(gate.observe(isSpeech: true) == .none)
    #expect(gate.observe(isSpeech: true) == .none)
    gate.resetForDiscontinuity()
    #expect(gate.observe(isSpeech: true) == .none)
    #expect(gate.observe(isSpeech: true) == .none)
    #expect(gate.observe(isSpeech: true) == .started)
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
func networkTrapPositiveControlInterceptsURLSessionWithoutNetwork() async throws {
    NetworkTrapURLProtocol.reset()
    let configuration = URLSessionConfiguration.ephemeral
    configuration.protocolClasses = [NetworkTrapURLProtocol.self]
    let session = URLSession(configuration: configuration)

    await #expect(throws: (any Error).self) {
        _ = try await session.data(
            from: URL(string: "https://network-trap.invalid/tokenizer.json")!
        )
    }

    #expect(NetworkTrapURLProtocol.requestCount > 0)
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
            == .queued
    )
    await gate.waitUntilWaiting()
    #expect(
        pipeline.enqueue([
            RecognitionHypothesis(segmentID: 1, text: "one", engineFinal: false)
        ])
            == .queued
    )
    #expect(
        pipeline.enqueue([
            RecognitionHypothesis(segmentID: 2, text: "two", engineFinal: true)
        ])
            == .queued
    )
    await gate.open()
    _ = await pipeline.finish()

    #expect(await recorder.segmentIDs == [0, 1, 2])
}

@Test
func saturatedRecognitionMailboxEvictsPartialsButPreservesFinalOrder() async {
    let gate = AsyncGate()
    let recorder = HypothesisRecorder()
    let pipeline = OrderedRecognitionBatchPipeline(capacity: 3)
    pipeline.start { batch in
        if batch.first?.segmentID == 0 {
            await gate.wait()
        }
        await recorder.append(batch)
    }
    #expect(
        pipeline.enqueue([
            RecognitionHypothesis(segmentID: 0, text: "zero", engineFinal: true)
        ]) == .queued
    )
    await gate.waitUntilWaiting()

    #expect(
        pipeline.enqueue([
            RecognitionHypothesis(segmentID: 10, text: "old", engineFinal: false)
        ]) == .queued
    )
    #expect(
        pipeline.enqueue([
            RecognitionHypothesis(segmentID: 11, text: "new", engineFinal: false)
        ]) == .coalescedPartial
    )
    #expect(
        pipeline.enqueue([
            RecognitionHypothesis(segmentID: 1, text: "one", engineFinal: true)
        ]) == .queued
    )
    #expect(
        pipeline.enqueue([
            RecognitionHypothesis(segmentID: 2, text: "two", engineFinal: true)
        ]) == .queued
    )
    #expect(
        pipeline.enqueue([
            RecognitionHypothesis(segmentID: 3, text: "three", engineFinal: true)
        ]) == .evictedPartial
    )
    #expect(pipeline.pendingCount == 3)
    #expect(pipeline.maximumPendingCount == 3)

    await gate.open()
    #expect(await pipeline.finish() == .stopped)
    #expect(await recorder.segmentIDs == [0, 1, 2, 3])
}

@Test
func allFinalMailboxSaturationFailsTypedWithoutSilentLoss() async {
    let gate = AsyncGate()
    let recorder = HypothesisRecorder()
    let pipeline = OrderedRecognitionBatchPipeline(capacity: 2)
    let worker = pipeline.start { batch in
        if batch.first?.segmentID == 0 {
            await gate.wait()
        }
        await recorder.append(batch)
    }
    #expect(
        pipeline.enqueue([
            RecognitionHypothesis(segmentID: 0, text: "zero", engineFinal: true)
        ]) == .queued
    )
    await gate.waitUntilWaiting()
    for segmentID in 1...2 {
        #expect(
            pipeline.enqueue([
                RecognitionHypothesis(
                    segmentID: UInt64(segmentID),
                    text: "\(segmentID)",
                    engineFinal: true
                )
            ]) == .queued
        )
    }
    #expect(
        pipeline.enqueue([
            RecognitionHypothesis(segmentID: 3, text: "three", engineFinal: true)
        ]) == .finalOverflow
    )

    await gate.open()
    #expect(
        await worker.value
            == .failed(
                SidecarServiceFailure(
                    stage: .speechRecognizer,
                    code: .recognitionFailed
                )
            )
    )
    #expect(await recorder.segmentIDs == [0, 1, 2])
    #expect(pipeline.maximumPendingCount == 2)
}

@Test
func voiceWindowPublicationFailureIsReportedAfterWorkerExit() async {
    let relay = RecognitionEventRelay()
    await relay.setHandler { event in
        guard case .voiceWindow = event else {
            return false
        }
        throw FatalTestError.rejectedPublication
    }
    let stopState = RecognitionWorkerStopState()
    let worker = Task {
        await runRecognitionWorker(stopState: stopState) {
            _ = try await relay.emit(
                .voiceWindow(
                    isSpeech: true,
                    frameMilliseconds: 100,
                    atMilliseconds: 100
                )
            )
        }
    }
    let recorder = WorkerFailureRecorder()
    let monitor = monitorRecognitionWorker(worker) { failure in
        let completion = await worker.value
        await recorder.record(
            failure: failure,
            observedCompletion: completion
        )
    }

    await monitor.value
    #expect(
        await recorder.failure
            == SidecarServiceFailure(
                stage: .speechRecognizer,
                code: .recognitionFailed
            )
    )
    #expect(
        await recorder.observedCompletion
            == .failed(
                SidecarServiceFailure(
                    stage: .speechRecognizer,
                    code: .recognitionFailed
                )
            )
    )
}

@Test
func unexpectedRecognitionWorkerCompletionCannotSelfAwait() async {
    let stopState = RecognitionWorkerStopState()
    let worker = Task {
        await runRecognitionWorker(stopState: stopState) {}
    }
    let recorder = WorkerFailureRecorder()
    let monitor = monitorRecognitionWorker(worker) { failure in
        let completion = await worker.value
        await recorder.record(
            failure: failure,
            observedCompletion: completion
        )
    }

    await monitor.value
    #expect(
        await recorder.observedCompletion
            == .failed(
                SidecarServiceFailure(
                    stage: .speechRecognizer,
                    code: .recognitionFailed
                )
            )
    )
}

@Test
func realSessionPublicationFailureCleansUpAfterRecognitionWorkerExit() async throws {
    let audio = FatalRecordingAudioService()
    let recognition = SessionOwnedRecognitionWorker()
    let playback = IntegrationPlaybackService()
    let events = FailOnceEventSink()
    let session = SidecarSession(
        audioService: audio,
        recognitionService: recognition,
        playbackService: playback,
        eventSink: events
    )
    let exitSignal = LockedSignal()
    let controller = SidecarFailureController(
        session: session,
        exitHandler: {
            exitSignal.signal()
        }
    )
    await recognition.attach(
        publisher: SidecarRecognitionEventPublisher(session: session),
        failureController: controller
    )
    try await startFatalSession(session)
    await events.failNext()

    #expect(
        await recognition.publish(
            RecognitionHypothesis(
                segmentID: 1,
                text: "rejected",
                engineFinal: true
            )
        ) == .queued
    )
    #expect(exitSignal.wait() == .success)

    #expect(await recognition.stopCount == 1)
    #expect(await recognition.stopObservedExitedWorker)
    #expect(await audio.stopCount == 1)
    #expect(await playback.stopCount == 1)
    #expect(
        await events.frames.filter {
            if case .failure = $0.control {
                return true
            }
            return false
        }
            == [
                ChildFrame(
                    control: .failure(
                        sessionID: 7,
                        stage: .speechRecognizer,
                        code: .recognitionFailed
                    )
                )
            ]
    )
}

@Test
func speakingGapPublishesEndedBeforeResetAndLaterSilenceDoesNotDuplicate() async throws {
    let playback = IntegrationPlaybackService()
    let events = FailOnceEventSink()
    let session = SidecarSession(
        audioService: FatalRecordingAudioService(),
        recognitionService: FatalRecordingRecognitionService(),
        playbackService: playback,
        eventSink: events
    )
    try await startFatalSession(session)
    try await session.handleMedia(
        ChildFrame(
            audioSessionID: 7,
            frame: pcmFrame(generationID: 5)
        )
    )
    let publisher = SidecarRecognitionEventPublisher(session: session)
    var gate = RecognitionSpeechGate(thresholdMilliseconds: 300)
    #expect(gate.observe(isSpeech: true) == .none)
    #expect(
        try await !publisher.publish(
            .voiceWindow(
                isSpeech: true,
                frameMilliseconds: 100,
                atMilliseconds: 100
            )
        )
    )
    #expect(gate.observe(isSpeech: true) == .none)
    #expect(
        try await publisher.publish(
            .voiceWindow(
                isSpeech: true,
                frameMilliseconds: 100,
                atMilliseconds: 200
            )
        )
    )

    #expect(!gate.isSpeaking)
    _ = try await publisher.publish(
        .captureDiscontinuity(atMilliseconds: 300)
    )
    gate.resetForDiscontinuity()
    #expect(gate.observe(isSpeech: false) == .none)

    let activities: [VoiceActivity] = await events.frames.compactMap { frame in
        guard
            case .voiceActivity(
                sessionID: _,
                activity: let activity
            ) = frame.control
        else {
            return nil
        }
        return activity
    }
    #expect(
        activities
            == [
                .speechStarted(atMilliseconds: 200),
                .speechEnded(atMilliseconds: 300),
            ]
    )
}

@Test(
    arguments: [UInt64(100), UInt64(300)],
    [false, true]
)
func sustainedSpeechPublishesOneLifecycleAcrossGateWindows(
    speechStartMilliseconds: UInt64,
    playbackActive: Bool
) async throws {
    let playback = IntegrationPlaybackService()
    let events = FailOnceEventSink()
    let session = SidecarSession(
        audioService: FatalRecordingAudioService(),
        recognitionService: FatalRecordingRecognitionService(),
        playbackService: playback,
        eventSink: events
    )
    try await startFatalSession(
        session,
        speechStartMilliseconds: speechStartMilliseconds
    )
    if playbackActive {
        try await session.handleMedia(
            ChildFrame(
                audioSessionID: 7,
                frame: pcmFrame(generationID: 5)
            )
        )
    }
    let publisher = SidecarRecognitionEventPublisher(session: session)
    var gate = RecognitionSpeechGate(
        thresholdMilliseconds: speechStartMilliseconds
    )

    let requiredWindows = Int(speechStartMilliseconds / 100)
    let firstFlushes: [UInt64] = playbackActive ? [5] : []
    for window in 1...4 {
        gate = try await publishRecognitionWindow(
            isSpeech: true,
            atMilliseconds: UInt64(window * 100),
            gate: gate,
            publisher: publisher
        )
        #expect(
            await playback.flushedGenerationIDs
                == (window >= requiredWindows ? firstFlushes : [])
        )
    }
    _ = try await publisher.publish(
        .captureDiscontinuity(atMilliseconds: 500)
    )
    gate.resetForDiscontinuity()
    gate = try await publishRecognitionWindow(
        isSpeech: false,
        atMilliseconds: 600,
        gate: gate,
        publisher: publisher
    )

    if playbackActive {
        try await session.handleMedia(
            ChildFrame(
                audioSessionID: 7,
                frame: pcmFrame(generationID: 6)
            )
        )
    }
    let secondFlushes: [UInt64] = playbackActive ? [5, 6] : []
    for window in 1...4 {
        gate = try await publishRecognitionWindow(
            isSpeech: true,
            atMilliseconds: UInt64(600 + window * 100),
            gate: gate,
            publisher: publisher
        )
        #expect(
            await playback.flushedGenerationIDs
                == (window >= requiredWindows ? secondFlushes : firstFlushes)
        )
    }
    _ = try await publishRecognitionWindow(
        isSpeech: false,
        atMilliseconds: 1_100,
        gate: gate,
        publisher: publisher
    )

    let firstStart = speechStartMilliseconds
    let secondStart = firstStart + 600
    let lifecycle = speechLifecycle(in: await events.frames)
    #expect(
        lifecycle
            == [
                .speechStarted(atMilliseconds: firstStart),
                .speechEnded(atMilliseconds: 500),
                .speechStarted(atMilliseconds: secondStart),
                .speechEnded(atMilliseconds: 1_100),
            ]
    )
}

@Test
func sessionBargeInRequiresTwoPostGapPlaybackActiveWindows() async throws {
    let playback = IntegrationPlaybackService()
    let events = FailOnceEventSink()
    let session = SidecarSession(
        audioService: FatalRecordingAudioService(),
        recognitionService: FatalRecordingRecognitionService(),
        playbackService: playback,
        eventSink: events
    )
    try await startFatalSession(session)
    try await session.handleMedia(
        ChildFrame(
            audioSessionID: 7,
            frame: pcmFrame(generationID: 5)
        )
    )
    let publisher = SidecarRecognitionEventPublisher(session: session)

    #expect(
        try await !publisher.publish(
            .voiceWindow(
                isSpeech: true,
                frameMilliseconds: 100,
                atMilliseconds: 100
            )
        )
    )
    #expect(
        try await !publisher.publish(
            .captureDiscontinuity(atMilliseconds: 200)
        )
    )
    #expect(
        try await !publisher.publish(
            .voiceWindow(
                isSpeech: true,
                frameMilliseconds: 100,
                atMilliseconds: 300
            )
        )
    )
    #expect(await playback.flushedGenerationIDs.isEmpty)

    #expect(
        try await publisher.publish(
            .voiceWindow(
                isSpeech: true,
                frameMilliseconds: 100,
                atMilliseconds: 400
            )
        )
    )
    #expect(await playback.flushedGenerationIDs == [5])
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

@Test
func outOfOrderRenderedCallbackUsesTypedFatalPath() async throws {
    let scheduler = RecordingPlaybackScheduler()
    let rendered = RenderedRecorder()
    let failures = PlaybackFailureRecorder()
    let playback = ContinuousPCMPlayback(scheduler: scheduler)
    await playback.setRenderedHandler { identity in
        await rendered.append(identity)
    }
    await playback.setFailureHandler { failure in
        await failures.record(failure)
    }
    try await playback.enqueue(
        pcmFrame(generationID: 9, sequence: 0)
    )
    try await playback.enqueue(
        pcmFrame(generationID: 9, sequence: 1)
    )

    scheduler.completeScheduledBuffer(at: 1)
    await failures.waitForFailure()

    #expect(await rendered.identities.isEmpty)
    #expect(
        await failures.failure
            == SidecarServiceFailure(
                stage: .audioOutput,
                code: .playbackFailed
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

private func publishRecognitionWindow(
    isSpeech: Bool,
    atMilliseconds: UInt64,
    gate: RecognitionSpeechGate,
    publisher: SidecarRecognitionEventPublisher
) async throws -> RecognitionSpeechGate {
    var gate = gate
    _ = try await publisher.publish(
        .voiceWindow(
            isSpeech: isSpeech,
            frameMilliseconds: 100,
            atMilliseconds: atMilliseconds
        )
    )
    let activity: VoiceActivity
    switch gate.observe(isSpeech: isSpeech) {
    case .none:
        return gate
    case .started:
        activity = .speechStarted(atMilliseconds: atMilliseconds)
    case .continued:
        activity = .speechContinued(atMilliseconds: atMilliseconds)
    case .ended:
        activity = .speechEnded(atMilliseconds: atMilliseconds)
    }
    _ = try await publisher.publish(.activity(activity))
    return gate
}

private func speechLifecycle(in frames: [ChildFrame]) -> [VoiceActivity] {
    frames.compactMap { frame in
        guard
            case .voiceActivity(
                sessionID: _,
                activity: let activity
            ) = frame.control
        else {
            return nil
        }
        switch activity {
        case .speechStarted, .speechEnded:
            return activity
        case .speechContinued:
            return nil
        }
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

private actor PlaybackFailureRecorder {
    private(set) var failure: SidecarServiceFailure?
    private var waiters: [CheckedContinuation<Void, Never>] = []

    func record(_ failure: SidecarServiceFailure) {
        self.failure = failure
        let values = waiters
        waiters.removeAll()
        for waiter in values {
            waiter.resume()
        }
    }

    func waitForFailure() async {
        guard failure == nil else {
            return
        }
        await withCheckedContinuation { continuation in
            waiters.append(continuation)
        }
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
    private var isWaiting = false
    private var waiters: [CheckedContinuation<Void, Never>] = []
    private var waitingObservers: [CheckedContinuation<Void, Never>] = []

    func wait() async {
        if isOpen {
            return
        }
        isWaiting = true
        let observers = waitingObservers
        waitingObservers.removeAll()
        for observer in observers {
            observer.resume()
        }
        await withCheckedContinuation { continuation in
            waiters.append(continuation)
        }
    }

    func waitUntilWaiting() async {
        guard !isWaiting else {
            return
        }
        await withCheckedContinuation { continuation in
            waitingObservers.append(continuation)
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

private actor WorkerFailureRecorder {
    private(set) var failure: SidecarServiceFailure?
    private(set) var observedCompletion: RecognitionWorkerCompletion?

    func record(
        failure: SidecarServiceFailure,
        observedCompletion: RecognitionWorkerCompletion
    ) {
        self.failure = failure
        self.observedCompletion = observedCompletion
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

    func prepare(configuration _: SidecarConfiguration) {}

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

private actor IntegrationPlaybackService: SidecarPlaybackService {
    private(set) var flushedGenerationIDs: [UInt64] = []
    private(set) var stopCount = 0

    func enqueue(_: PCMFrame) {}

    func flush(throughGenerationID generationID: UInt64) {
        flushedGenerationIDs.append(generationID)
    }

    func stop() {
        stopCount += 1
    }
}

private actor SessionOwnedRecognitionWorker: SidecarRecognitionService {
    private var publisher: SidecarRecognitionEventPublisher?
    private var failureController: SidecarFailureController?
    private var pipeline: OrderedRecognitionBatchPipeline?
    private var monitor: Task<Void, Never>?
    private let monitorObservedExit = LockedFlag()
    private(set) var stopCount = 0
    private(set) var stopObservedExitedWorker = false

    func attach(
        publisher: SidecarRecognitionEventPublisher,
        failureController: SidecarFailureController
    ) {
        self.publisher = publisher
        self.failureController = failureController
    }

    func prepare(configuration _: SidecarConfiguration) {}

    func start(configuration: SidecarConfiguration) throws {
        guard let publisher, let failureController else {
            throw SidecarServiceFailure(
                stage: .speechRecognizer,
                code: .invalidState
            )
        }
        let pipeline = OrderedRecognitionBatchPipeline(capacity: 2)
        let worker = pipeline.start { hypotheses in
            for hypothesis in hypotheses {
                _ = try await publisher.publish(
                    .hypothesis(hypothesis)
                )
            }
        }
        self.pipeline = pipeline
        let monitorObservedExit = monitorObservedExit
        monitor = monitorRecognitionWorker(worker) { failure in
            monitorObservedExit.set()
            await failureController.terminate(
                with: failure,
                fallbackSessionID: configuration.sessionID
            )
        }
    }

    func stop() async {
        stopCount += 1
        stopObservedExitedWorker = monitorObservedExit.value
        await pipeline?.cancel()
        pipeline = nil
        monitor?.cancel()
        monitor = nil
    }

    func publish(
        _ hypothesis: RecognitionHypothesis
    ) -> RecognitionBatchEnqueueResult {
        pipeline?.enqueue([hypothesis]) ?? .terminated
    }
}

private final class LockedFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var isSet = false

    var value: Bool {
        lock.withLock { isSet }
    }

    func set() {
        lock.withLock {
            isSet = true
        }
    }
}

private final class LockedSignal: @unchecked Sendable {
    private let semaphore = DispatchSemaphore(value: 0)

    func signal() {
        semaphore.signal()
    }

    func wait() -> DispatchTimeoutResult {
        semaphore.wait(timeout: .now() + 2)
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

private func startFatalSession(
    _ session: SidecarSession,
    speechStartMilliseconds: UInt64 = 200
) async throws {
    try await session.handleControl(
        ChildFrame(
            control: .startSession(
                sessionID: 7,
                speechStartMilliseconds: speechStartMilliseconds,
                finalSilenceMilliseconds: 600
            )
        )
    )
    try await session.handleControl(
        ChildFrame(control: .startCapture(sessionID: 7))
    )
}

private func pcmFrame(
    generationID: UInt64,
    sequence: UInt64 = 0
) throws -> PCMFrame {
    let sample: Float = 0
    return try PCMFrame(
        turnID: generationID,
        generationID: generationID,
        utteranceID: 1,
        sequence: sequence,
        format: PCMFormat(
            sampleRateHz: 16_000,
            channels: 1,
            sampleFormat: .float32LittleEndian
        ),
        bytes: withUnsafeBytes(of: sample) { Data($0) }
    )
}
