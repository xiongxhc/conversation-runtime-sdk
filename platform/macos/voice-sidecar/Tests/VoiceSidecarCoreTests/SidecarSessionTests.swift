import Testing
@testable import VoiceSidecarCore

@Test
func startSessionSnapshotsThresholdsAndEmitsReady() async throws {
    let audio = RecordingAudioService()
    let recognition = RecordingRecognitionService()
    let playback = RecordingPlaybackService()
    let events = RecordingEventSink()
    let session = SidecarSession(
        audioService: audio,
        recognitionService: recognition,
        playbackService: playback,
        eventSink: events
    )

    try await session.handleControl(
        ChildFrame(
            control: .startSession(
                sessionID: 7,
                speechStartMilliseconds: 200,
                finalSilenceMilliseconds: 600
            )
        )
    )

    #expect(await audio.configurations.isEmpty)
    #expect(await recognition.configurations.isEmpty)
    #expect(
        await events.frames
            == [ChildFrame(control: .ready(sessionID: 7))]
    )
}

@Test
func injectedServicesStartOnlyAfterStartCapture() async throws {
    let callLog = CallLog()
    let audio = RecordingAudioService(callLog: callLog)
    let recognition = RecordingRecognitionService(callLog: callLog)
    let session = SidecarSession(
        audioService: audio,
        recognitionService: recognition,
        playbackService: RecordingPlaybackService(),
        eventSink: RecordingEventSink()
    )

    try await startSession(session)
    #expect(await audio.configurations.isEmpty)
    #expect(await recognition.preparedConfigurations.isEmpty)
    #expect(await recognition.configurations.isEmpty)
    await callLog.clear()

    try await session.handleControl(
        ChildFrame(control: .startCapture(sessionID: 7))
    )

    let expected = SidecarConfiguration(
        sessionID: 7,
        speechStartMilliseconds: 200,
        finalSilenceMilliseconds: 600
    )
    #expect(await audio.configurations == [expected])
    #expect(await recognition.preparedConfigurations == [expected])
    #expect(await recognition.configurations == [expected])
    #expect(
        await callLog.entries
            == [
                "recognition.prepare",
                "audio.start",
                "recognition.start",
            ]
    )
}

@Test
func invalidThresholdsEmitTypedContentFreeFailure() async {
    let events = RecordingEventSink()
    let session = SidecarSession(
        audioService: RecordingAudioService(),
        recognitionService: RecordingRecognitionService(),
        playbackService: RecordingPlaybackService(),
        eventSink: events
    )

    do {
        try await session.handleControl(
            ChildFrame(
                control: .startSession(
                    sessionID: 7,
                    speechStartMilliseconds: 99,
                    finalSilenceMilliseconds: 600
                )
            )
        )
        Issue.record("expected invalid speech threshold")
    } catch let error as SidecarSessionError {
        #expect(error == .invalidSpeechStartThreshold)
    } catch {
        Issue.record("unexpected error \(error)")
    }

    #expect(
        await events.frames
            == [
                ChildFrame(
                    control: .failure(
                        sessionID: 7,
                        stage: .voiceSidecar,
                        code: .invalidState
                    )
                )
            ]
    )
}

@Test
func acceptedPCMUsesExactIdentityAndSession() async throws {
    let playback = RecordingPlaybackService()
    let events = RecordingEventSink()
    let session = SidecarSession(
        audioService: RecordingAudioService(),
        recognitionService: RecordingRecognitionService(),
        playbackService: playback,
        eventSink: events
    )
    try await startAndCapture(session)
    let frame = try pcmFrame(generationID: 3, utteranceID: 4, sequence: 0)

    try await session.handleMedia(ChildFrame(audioSessionID: 7, frame: frame))

    #expect(await playback.frames == [frame])
    #expect(
        await events.frames.last
            == ChildFrame(
                control: .playbackAccepted(
                    sessionID: 7,
                    turnID: 3,
                    generationID: 3,
                    utteranceID: 4,
                    sequence: 0
                )
            )
    )
}

@Test
func flushedRenderedCallbackIsTheOnlyIgnoredOutputCallback() async throws {
    let events = RecordingEventSink()
    let session = SidecarSession(
        audioService: RecordingAudioService(),
        recognitionService: RecordingRecognitionService(),
        playbackService: RecordingPlaybackService(),
        eventSink: events
    )
    try await startAndCapture(session)
    let frame = try pcmFrame(generationID: 5)
    try await session.handleMedia(
        ChildFrame(audioSessionID: 7, frame: frame)
    )
    try await session.handleControl(
        ChildFrame(
            control: .flushGeneration(
                sessionID: 7,
                generationID: 5,
                operationID: 9
            )
        )
    )
    let framesBeforeLateCallback = await events.frames

    try await session.playbackRendered(frame.identity)

    #expect(await events.frames == framesBeforeLateCallback)
}

@Test
func nonStaleRenderedOrderFailureIsFatal() async throws {
    let audio = RecordingAudioService()
    let recognition = RecordingRecognitionService()
    let playback = RecordingPlaybackService()
    let events = RecordingEventSink()
    let session = SidecarSession(
        audioService: audio,
        recognitionService: recognition,
        playbackService: playback,
        eventSink: events
    )
    try await startAndCapture(session)
    let first = try pcmFrame(generationID: 5, sequence: 0)
    let second = try pcmFrame(generationID: 5, sequence: 1)
    try await session.handleMedia(
        ChildFrame(audioSessionID: 7, frame: first)
    )
    try await session.handleMedia(
        ChildFrame(audioSessionID: 7, frame: second)
    )

    await #expect(throws: PlaybackBufferError.renderOrderMismatch) {
        try await session.playbackRendered(second.identity)
    }

    #expect(await audio.stopCount == 1)
    #expect(await recognition.stopCount == 1)
    #expect(await playback.stopCount == 1)
    #expect(
        await events.frames.last
            == ChildFrame(
                control: .failure(
                    sessionID: 7,
                    stage: .voiceSidecar,
                    code: .malformedFrame
                )
            )
    )
}

@Test
func suspendedEnqueueCannotResurrectFlushedGenerationOrEmitLateAccepted() async throws {
    let playback = ControllablePlaybackService()
    await playback.setSuspendEnqueue(true)
    let events = RecordingEventSink()
    let session = SidecarSession(
        audioService: RecordingAudioService(),
        recognitionService: RecordingRecognitionService(),
        playbackService: playback,
        eventSink: events
    )
    try await startAndCapture(session)
    let frame = try pcmFrame(generationID: 3)

    let enqueue = Task {
        try await session.handleMedia(
            ChildFrame(audioSessionID: 7, frame: frame)
        )
    }
    await playback.waitUntilEnqueueStarted()

    try await session.handleControl(
        ChildFrame(
            control: .flushGeneration(
                sessionID: 7,
                generationID: 3,
                operationID: 9
            )
        )
    )
    await playback.releaseEnqueues()
    try await enqueue.value

    #expect(
        await events.frames
            == [
                ChildFrame(control: .ready(sessionID: 7)),
                ChildFrame(
                    control: .playbackFlushed(
                        sessionID: 7,
                        generationID: 3,
                        operationID: 9
                    )
                ),
            ]
    )

    do {
        try await session.handleMedia(
            ChildFrame(audioSessionID: 7, frame: frame)
        )
        Issue.record("expected flushed generation to remain stale")
    } catch let error as PlaybackBufferError {
        #expect(error == .staleGeneration)
    } catch {
        Issue.record("unexpected error \(error)")
    }
}

@Test
func mismatchedSessionFailsWithoutCallingPlayback() async throws {
    let playback = RecordingPlaybackService()
    let events = RecordingEventSink()
    let session = SidecarSession(
        audioService: RecordingAudioService(),
        recognitionService: RecordingRecognitionService(),
        playbackService: playback,
        eventSink: events
    )
    try await startAndCapture(session)

    do {
        try await session.handleMedia(
            ChildFrame(audioSessionID: 8, frame: pcmFrame())
        )
        Issue.record("expected session mismatch")
    } catch let error as SidecarSessionError {
        #expect(error == .sessionMismatch)
    }

    #expect(await playback.frames.isEmpty)
    #expect(
        await events.frames.last
            == ChildFrame(
                control: .failure(
                    sessionID: 7,
                    stage: .voiceSidecar,
                    code: .invalidState
                )
            )
    )
}

@Test
func localFlushPrecedesBargeInActivity() async throws {
    let callLog = CallLog()
    let playback = RecordingPlaybackService(callLog: callLog)
    let events = RecordingEventSink(callLog: callLog)
    let session = SidecarSession(
        audioService: RecordingAudioService(),
        recognitionService: RecordingRecognitionService(),
        playbackService: playback,
        eventSink: events
    )
    try await startAndCapture(session)
    try await session.handleMedia(
        ChildFrame(audioSessionID: 7, frame: pcmFrame(generationID: 5))
    )
    await callLog.clear()

    #expect(
        try await session.observeBargeIn(
            isSpeech: true,
            frameMilliseconds: 100,
            atMilliseconds: 10
        ) == false
    )
    #expect(
        try await session.observeBargeIn(
            isSpeech: true,
            frameMilliseconds: 100,
            atMilliseconds: 110
        ) == true
    )

    #expect(
        await callLog.entries
            == ["playback.flush.5", "event.voiceActivity"]
    )
}

@Test(arguments: [
    (UInt64(100), 1),
    (UInt64(200), 2),
    (UInt64(1_000), 10),
])
func bargeInHonorsConfiguredSpeechStartThreshold(
    speechStartMilliseconds: UInt64,
    requiredWindows: Int
) async throws {
    let playback = RecordingPlaybackService()
    let session = SidecarSession(
        audioService: RecordingAudioService(),
        recognitionService: RecordingRecognitionService(),
        playbackService: playback,
        eventSink: RecordingEventSink()
    )
    try await startAndCapture(
        session,
        speechStartMilliseconds: speechStartMilliseconds
    )
    try await session.handleMedia(
        ChildFrame(audioSessionID: 7, frame: pcmFrame(generationID: 5))
    )

    for index in 1..<requiredWindows {
        #expect(
            try await session.observeBargeIn(
                isSpeech: true,
                frameMilliseconds: 100,
                atMilliseconds: UInt64(index * 100)
            ) == false
        )
        #expect(await playback.flushedGenerations.isEmpty)
    }
    #expect(
        try await session.observeBargeIn(
            isSpeech: true,
            frameMilliseconds: 100,
            atMilliseconds: UInt64(requiredWindows * 100)
        ) == true
    )
    #expect(await playback.flushedGenerations == [5])
}

@Test
func playbackInactiveSpeechCannotTriggerLocalFlush() async throws {
    let playback = RecordingPlaybackService()
    let events = RecordingEventSink()
    let session = SidecarSession(
        audioService: RecordingAudioService(),
        recognitionService: RecordingRecognitionService(),
        playbackService: playback,
        eventSink: events
    )
    try await startAndCapture(session)

    #expect(
        try await session.observeBargeIn(
            isSpeech: true,
            frameMilliseconds: 100,
            atMilliseconds: 10
        ) == false
    )
    #expect(
        try await session.observeBargeIn(
            isSpeech: true,
            frameMilliseconds: 100,
            atMilliseconds: 110
        ) == false
    )

    #expect(await playback.flushedGenerations.isEmpty)
    #expect(await events.frames.count == 1)
}

@Test
func parentFlushCompletesLocallyBeforeAcknowledgement() async throws {
    let callLog = CallLog()
    let playback = RecordingPlaybackService(callLog: callLog)
    let events = RecordingEventSink(callLog: callLog)
    let session = SidecarSession(
        audioService: RecordingAudioService(),
        recognitionService: RecordingRecognitionService(),
        playbackService: playback,
        eventSink: events
    )
    try await startAndCapture(session)
    await callLog.clear()

    try await session.handleControl(
        ChildFrame(
            control: .flushGeneration(
                sessionID: 7,
                generationID: 9,
                operationID: 11
            )
        )
    )

    #expect(
        await callLog.entries
            == ["playback.flush.9", "event.playbackFlushed"]
    )
}

@Test
func suspendedFlushAndShutdownCannotInterleaveDuplicateLifecycle() async throws {
    let playback = ControllablePlaybackService()
    await playback.setSuspendFlush(true)
    let events = RecordingEventSink()
    let session = SidecarSession(
        audioService: RecordingAudioService(),
        recognitionService: RecordingRecognitionService(),
        playbackService: playback,
        eventSink: events
    )
    try await startAndCapture(session)

    let firstFlush = Task {
        try await session.handleControl(
            ChildFrame(
                control: .flushGeneration(
                    sessionID: 7,
                    generationID: 3,
                    operationID: 1
                )
            )
        )
    }
    await playback.waitUntilFlushStarted()

    let secondDone = CompletionFlag()
    let secondFlush = Task { () -> SidecarSessionError? in
        do {
            try await session.handleControl(
                ChildFrame(
                    control: .flushGeneration(
                        sessionID: 7,
                        generationID: 4,
                        operationID: 2
                    )
                )
            )
            await secondDone.complete()
            return nil
        } catch let error as SidecarSessionError {
            await secondDone.complete()
            return error
        } catch {
            await secondDone.complete()
            Issue.record("unexpected error \(error)")
            return nil
        }
    }
    let shutdownDone = CompletionFlag()
    let concurrentShutdown = Task { () -> SidecarSessionError? in
        do {
            try await session.handleControl(
                ChildFrame(control: .shutdown(sessionID: 7))
            )
            await shutdownDone.complete()
            return nil
        } catch let error as SidecarSessionError {
            await shutdownDone.complete()
            return error
        } catch {
            await shutdownDone.complete()
            Issue.record("unexpected error \(error)")
            return nil
        }
    }

    #expect(
        await waitUntil {
            let shutdownComplete = await shutdownDone.isComplete
            let flushCount = await playback.flushedGenerations.count
            return shutdownComplete || flushCount == 2
        }
    )
    await playback.releaseFlushes()
    try await firstFlush.value

    #expect(await secondFlush.value == .invalidState)
    #expect(await concurrentShutdown.value == .invalidState)
    #expect(await secondDone.isComplete)
    #expect(await playback.flushedGenerations == [3])
    #expect(
        await events.frames
            == [
                ChildFrame(control: .ready(sessionID: 7)),
                ChildFrame(
                    control: .playbackFlushed(
                        sessionID: 7,
                        generationID: 3,
                        operationID: 1
                    )
                ),
            ]
    )

    try await session.handleControl(
        ChildFrame(control: .shutdown(sessionID: 7))
    )
    #expect(await playback.stopCount == 1)
    #expect(await events.frames.last == ChildFrame(control: .shutdownComplete(sessionID: 7)))
}

@Test
func startAndShutdownCannotInterleaveDuplicateLifecycle() async throws {
    let audio = SuspendingAudioService()
    let recognition = RecordingRecognitionService()
    let events = RecordingEventSink()
    let session = SidecarSession(
        audioService: audio,
        recognitionService: recognition,
        playbackService: RecordingPlaybackService(),
        eventSink: events
    )
    try await startSession(session)

    let firstStart = Task {
        try await session.handleControl(
            ChildFrame(control: .startCapture(sessionID: 7))
        )
    }
    await audio.waitUntilStarted()

    let duplicateDone = CompletionFlag()
    let duplicateStart = Task { () -> SidecarSessionError? in
        do {
            try await session.handleControl(
                ChildFrame(control: .startCapture(sessionID: 7))
            )
            await duplicateDone.complete()
            return nil
        } catch let error as SidecarSessionError {
            await duplicateDone.complete()
            return error
        } catch {
            await duplicateDone.complete()
            Issue.record("unexpected error \(error)")
            return nil
        }
    }
    let shutdownDone = CompletionFlag()
    let concurrentShutdown = Task { () -> SidecarSessionError? in
        do {
            try await session.handleControl(
                ChildFrame(control: .shutdown(sessionID: 7))
            )
            await shutdownDone.complete()
            return nil
        } catch let error as SidecarSessionError {
            await shutdownDone.complete()
            return error
        } catch {
            await shutdownDone.complete()
            Issue.record("unexpected error \(error)")
            return nil
        }
    }

    #expect(
        await waitUntil {
            let shutdownComplete = await shutdownDone.isComplete
            let startCount = await audio.startCount
            return shutdownComplete || startCount == 2
        }
    )
    await audio.releaseStarts()
    try await firstStart.value

    #expect(await duplicateStart.value == .invalidState)
    #expect(await concurrentShutdown.value == .invalidState)
    #expect(await duplicateDone.isComplete)
    #expect(await audio.startCount == 1)
    #expect(await recognition.configurations.count == 1)
    #expect(await events.frames == [ChildFrame(control: .ready(sessionID: 7))])

    try await session.handleControl(
        ChildFrame(control: .shutdown(sessionID: 7))
    )
    #expect(await audio.stopCount == 1)
    #expect(await recognition.stopCount == 1)
    #expect(await session.isTerminated)
}

@Test
func hypothesesAreForwardedWithoutConversationalFinalization() async throws {
    let events = RecordingEventSink()
    let session = SidecarSession(
        audioService: RecordingAudioService(),
        recognitionService: RecordingRecognitionService(),
        playbackService: RecordingPlaybackService(),
        eventSink: events
    )
    try await startAndCapture(session)
    let hypothesis = RecognitionHypothesis(segmentID: 3, text: "hello", engineFinal: true)

    try await session.publishRecognitionHypothesis(hypothesis)

    #expect(
        await events.frames.last
            == ChildFrame(
                control: .transcriptHypothesis(
                    sessionID: 7,
                    hypothesis: hypothesis
                )
            )
    )
}

@Test
func playbackFailureEmitsClosedTypedFailure() async throws {
    let audio = RecordingAudioService()
    let recognition = RecordingRecognitionService()
    let playback = RecordingPlaybackService()
    await playback.setEnqueueFailure(
        SidecarServiceFailure(stage: .audioOutput, code: .playbackFailed)
    )
    let events = RecordingEventSink()
    let session = SidecarSession(
        audioService: audio,
        recognitionService: recognition,
        playbackService: playback,
        eventSink: events
    )
    try await startAndCapture(session)

    do {
        try await session.handleMedia(
            ChildFrame(audioSessionID: 7, frame: pcmFrame())
        )
        Issue.record("expected playback failure")
    } catch let error as SidecarServiceFailure {
        #expect(error == SidecarServiceFailure(stage: .audioOutput, code: .playbackFailed))
    }

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
    #expect(await audio.stopCount == 1)
    #expect(await recognition.stopCount == 1)
    #expect(await playback.stopCount == 1)
}

@Test
func partialAudioStartFailureStopsAttemptedServiceOnceBeforeFailure() async throws {
    let callLog = CallLog()
    let audio = RecordingAudioService(callLog: callLog)
    let failure = SidecarServiceFailure(
        stage: .audioCapture,
        code: .audioDeviceUnavailable
    )
    await audio.setStartFailure(failure)
    let recognition = RecordingRecognitionService(callLog: callLog)
    let playback = RecordingPlaybackService(callLog: callLog)
    let events = RecordingEventSink(callLog: callLog)
    let session = SidecarSession(
        audioService: audio,
        recognitionService: recognition,
        playbackService: playback,
        eventSink: events
    )
    try await startSession(session)
    await callLog.clear()

    do {
        try await session.handleControl(
            ChildFrame(control: .startCapture(sessionID: 7))
        )
        Issue.record("expected partial audio startup failure")
    } catch let error as SidecarServiceFailure {
        #expect(error == failure)
    } catch {
        Issue.record("unexpected error \(error)")
    }

    #expect(
        await callLog.entries
            == [
                "recognition.prepare",
                "audio.start",
                "audio.stop",
                "recognition.stop",
                "playback.stop",
                "event.failure",
            ]
    )
    #expect(await audio.stopCount == 1)
    #expect(await recognition.stopCount == 1)
    #expect(await playback.stopCount == 1)
}

@Test
func recognitionPreflightFailureDoesNotActivateCapture() async throws {
    let callLog = CallLog()
    let audio = RecordingAudioService(callLog: callLog)
    let recognition = RecordingRecognitionService(callLog: callLog)
    let failure = SidecarServiceFailure(
        stage: .speechRecognizer,
        code: .recognitionFailed
    )
    await recognition.setPrepareFailure(failure)
    let playback = RecordingPlaybackService(callLog: callLog)
    let events = RecordingEventSink(callLog: callLog)
    let session = SidecarSession(
        audioService: audio,
        recognitionService: recognition,
        playbackService: playback,
        eventSink: events
    )
    try await startSession(session)
    await callLog.clear()

    do {
        try await session.handleControl(
            ChildFrame(control: .startCapture(sessionID: 7))
        )
        Issue.record("expected recognition preflight failure")
    } catch let error as SidecarServiceFailure {
        #expect(error == failure)
    } catch {
        Issue.record("unexpected error \(error)")
    }

    #expect(
        await callLog.entries
            == [
                "recognition.prepare",
                "recognition.stop",
                "playback.stop",
                "event.failure",
            ]
    )
    #expect(await audio.configurations.isEmpty)
    #expect(await audio.stopCount == 0)
    #expect(await recognition.stopCount == 1)
    #expect(await playback.stopCount == 1)
}

@Test
func partialRecognitionStartFailureCleansUpInReverseOrderOnce() async throws {
    let callLog = CallLog()
    let audio = RecordingAudioService(callLog: callLog)
    let recognition = RecordingRecognitionService(callLog: callLog)
    let failure = SidecarServiceFailure(
        stage: .speechRecognizer,
        code: .recognitionFailed
    )
    await recognition.setStartFailure(failure)
    let playback = RecordingPlaybackService(callLog: callLog)
    let events = RecordingEventSink(callLog: callLog)
    let session = SidecarSession(
        audioService: audio,
        recognitionService: recognition,
        playbackService: playback,
        eventSink: events
    )
    try await startSession(session)
    await callLog.clear()

    do {
        try await session.handleControl(
            ChildFrame(control: .startCapture(sessionID: 7))
        )
        Issue.record("expected partial recognition startup failure")
    } catch let error as SidecarServiceFailure {
        #expect(error == failure)
    } catch {
        Issue.record("unexpected error \(error)")
    }

    #expect(
        await callLog.entries
            == [
                "recognition.prepare",
                "audio.start",
                "recognition.start",
                "recognition.stop",
                "audio.stop",
                "playback.stop",
                "event.failure",
            ]
    )
    #expect(await audio.stopCount == 1)
    #expect(await recognition.stopCount == 1)
    #expect(await playback.stopCount == 1)
}

@Test
func shutdownStopsInjectedServicesAndEmitsCompletion() async throws {
    let audio = RecordingAudioService()
    let recognition = RecordingRecognitionService()
    let playback = RecordingPlaybackService()
    let events = RecordingEventSink()
    let session = SidecarSession(
        audioService: audio,
        recognitionService: recognition,
        playbackService: playback,
        eventSink: events
    )
    try await startAndCapture(session)

    try await session.handleControl(
        ChildFrame(control: .shutdown(sessionID: 7))
    )

    #expect(await audio.stopCount == 1)
    #expect(await recognition.stopCount == 1)
    #expect(await playback.stopCount == 1)
    #expect(await session.isTerminated)
    #expect(
        await events.frames.last
            == ChildFrame(control: .shutdownComplete(sessionID: 7))
    )
}

private func startSession(
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
}

private func startAndCapture(
    _ session: SidecarSession,
    speechStartMilliseconds: UInt64 = 200
) async throws {
    try await startSession(
        session,
        speechStartMilliseconds: speechStartMilliseconds
    )
    try await session.handleControl(
        ChildFrame(control: .startCapture(sessionID: 7))
    )
}
