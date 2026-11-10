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
        ChildFrame(control: .startCapture(sessionID: 7, operationID: 1))
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
func startCapturePublishesActiveAudioDeviceLabels() async throws {
    let devices = AudioDeviceStatus(
        inputLabel: "MacBook Pro Microphone",
        outputLabel: "Chris 的 AirPods"
    )
    let events = RecordingEventSink()
    let session = SidecarSession(
        audioService: RecordingDeviceAudioService(status: devices),
        recognitionService: RecordingRecognitionService(),
        playbackService: RecordingPlaybackService(),
        eventSink: events
    )
    try await startSession(session)

    try await session.handleControl(
        ChildFrame(control: .startCapture(sessionID: 7, operationID: 1))
    )

    #expect(
        await events.frames.suffix(2)
            == [
                ChildFrame(
                    control: .audioDeviceStatus(
                        sessionID: 7,
                        inputLabel: devices.inputLabel,
                        outputLabel: devices.outputLabel
                    )
                ),
                ChildFrame(
                    control: .captureStarted(
                        sessionID: 7,
                        operationID: 1
                    )
                ),
            ]
    )
}

@Test
func startCaptureSucceedsWhenTheDeviceLabelLookupFails() async throws {
    let events = RecordingEventSink()
    let session = SidecarSession(
        audioService: RecordingDeviceAudioService(status: nil),
        recognitionService: RecordingRecognitionService(),
        playbackService: RecordingPlaybackService(),
        eventSink: events
    )
    try await startSession(session)

    try await session.handleControl(
        ChildFrame(control: .startCapture(sessionID: 7, operationID: 1))
    )

    #expect(
        await events.frames
            == [
                ChildFrame(control: .ready(sessionID: 7)),
                ChildFrame(control: .captureStarted(sessionID: 7, operationID: 1)),
            ]
    )
}

@Test
func startCaptureSkipsBlankDeviceLabels() async throws {
    let events = RecordingEventSink()
    let session = SidecarSession(
        audioService: RecordingDeviceAudioService(
            status: AudioDeviceStatus(inputLabel: "  ", outputLabel: "Speakers")
        ),
        recognitionService: RecordingRecognitionService(),
        playbackService: RecordingPlaybackService(),
        eventSink: events
    )
    try await startSession(session)

    try await session.handleControl(
        ChildFrame(control: .startCapture(sessionID: 7, operationID: 1))
    )

    #expect(
        await events.frames
            == [
                ChildFrame(control: .ready(sessionID: 7)),
                ChildFrame(control: .captureStarted(sessionID: 7, operationID: 1)),
            ]
    )
}

@Test
func resumeCaptureRepublishesActiveAudioDeviceLabels() async throws {
    let devices = AudioDeviceStatus(
        inputLabel: "MacBook Pro Microphone",
        outputLabel: "Chris 的 AirPods"
    )
    let events = RecordingEventSink()
    let session = SidecarSession(
        audioService: RecordingDeviceAudioService(status: devices),
        recognitionService: RecordingRecognitionService(),
        playbackService: RecordingPlaybackService(),
        eventSink: events
    )
    try await startSession(session)
    try await session.handleControl(
        ChildFrame(control: .startCapture(sessionID: 7, operationID: 1))
    )
    try await session.handleControl(
        ChildFrame(control: .pauseCapture(sessionID: 7, operationID: 2))
    )

    try await session.handleControl(
        ChildFrame(control: .resumeCapture(sessionID: 7, operationID: 3))
    )

    #expect(
        await events.frames.suffix(2)
            == [
                ChildFrame(
                    control: .audioDeviceStatus(
                        sessionID: 7,
                        inputLabel: devices.inputLabel,
                        outputLabel: devices.outputLabel
                    )
                ),
                ChildFrame(control: .captureResumed(sessionID: 7, operationID: 3)),
            ]
    )
}

@Test
func pauseStopsCaptureBeforeAcknowledgementAndResumeRestartsIt() async throws {
    let callLog = CallLog()
    let audio = RecordingAudioService(callLog: callLog)
    let recognition = RecordingRecognitionService(callLog: callLog)
    let events = RecordingEventSink(callLog: callLog)
    let session = SidecarSession(
        audioService: audio,
        recognitionService: recognition,
        playbackService: RecordingPlaybackService(),
        eventSink: events
    )
    try await startSession(session)
    try await session.handleControl(
        ChildFrame(control: .startCapture(sessionID: 7, operationID: 1))
    )
    await callLog.clear()

    try await session.handleControl(
        ChildFrame(control: .pauseCapture(sessionID: 7, operationID: 2))
    )
    #expect(await audio.pauseCaptureCount == 1)
    #expect(
        await callLog.entries
            == ["recognition.stop", "audio.pause", "event.capturePaused"]
    )

    await callLog.clear()
    try await session.handleControl(
        ChildFrame(control: .resumeCapture(sessionID: 7, operationID: 3))
    )
    #expect(await audio.resumeCaptureCount == 1)
    #expect(await recognition.preparedConfigurations.count == 2)
    #expect(
        await callLog.entries
            == [
                "recognition.prepare",
                "audio.resume",
                "recognition.start",
                "event.captureResumed",
            ]
    )
    #expect(
        await events.frames.suffix(3)
            == [
                ChildFrame(control: .captureStarted(sessionID: 7, operationID: 1)),
                ChildFrame(control: .capturePaused(sessionID: 7, operationID: 2)),
                ChildFrame(control: .captureResumed(sessionID: 7, operationID: 3)),
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
func outOfOrderRenderedAcknowledgementResolvesTheOrderedPrefix() async throws {
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

    // Rendered acknowledgements cross several actors between the playback
    // engine and this session, so a later frame's acknowledgement can land
    // first. Playback renders strictly in order, so it proves the earlier
    // frame also finished.
    try await session.playbackRendered(second.identity)
    // The earlier frame's own acknowledgement is a duplicate by now.
    try await session.playbackRendered(first.identity)

    #expect(await audio.stopCount == 0)
    #expect(
        await events.frames.suffix(2)
            == [
                ChildFrame(
                    control: .playbackRendered(
                        sessionID: 7,
                        turnID: 5,
                        generationID: 5,
                        utteranceID: 1,
                        sequence: 0
                    )
                ),
                ChildFrame(
                    control: .playbackRendered(
                        sessionID: 7,
                        turnID: 5,
                        generationID: 5,
                        utteranceID: 1,
                        sequence: 1
                    )
                ),
            ]
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
                ChildFrame(control: .captureStarted(sessionID: 7, operationID: 1)),
                ChildFrame(
                    control: .playbackFlushed(
                        sessionID: 7,
                        generationID: 3,
                        operationID: 9
                    )
                ),
            ]
    )

    try await session.handleMedia(
        ChildFrame(audioSessionID: 7, frame: frame)
    )
    #expect(
        await events.frames
            == [
                ChildFrame(control: .ready(sessionID: 7)),
                ChildFrame(control: .captureStarted(sessionID: 7, operationID: 1)),
                ChildFrame(
                    control: .playbackFlushed(
                        sessionID: 7,
                        generationID: 3,
                        operationID: 9
                    )
                ),
            ]
    )
}

@Test
func frameArrivingAfterItsGenerationWasFlushedIsDroppedWithoutFailure() async throws {
    let playback = ControllablePlaybackService()
    let events = RecordingEventSink()
    let session = SidecarSession(
        audioService: RecordingAudioService(),
        recognitionService: RecordingRecognitionService(),
        playbackService: playback,
        eventSink: events
    )
    try await startAndCapture(session)
    try await session.handleControl(
        ChildFrame(
            control: .flushGeneration(
                sessionID: 7,
                generationID: 3,
                operationID: 9
            )
        )
    )

    try await session.handleMedia(
        ChildFrame(audioSessionID: 7, frame: pcmFrame(generationID: 3))
    )

    #expect(await playback.frames.isEmpty)
    #expect(
        await events.frames
            == [
                ChildFrame(control: .ready(sessionID: 7)),
                ChildFrame(control: .captureStarted(sessionID: 7, operationID: 1)),
                ChildFrame(
                    control: .playbackFlushed(
                        sessionID: 7,
                        generationID: 3,
                        operationID: 9
                    )
                ),
            ]
    )
}

@Test
func mediaFrameArrivingDuringSuspendedFlushIsDroppedWithoutFailure() async throws {
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
    try await session.handleMedia(
        ChildFrame(audioSessionID: 7, frame: pcmFrame(generationID: 3))
    )

    let flush = Task {
        try await session.handleControl(
            ChildFrame(
                control: .flushGeneration(
                    sessionID: 7,
                    generationID: 3,
                    operationID: 2
                )
            )
        )
    }
    await playback.waitUntilFlushStarted()

    // The media channel is independent of the control channel, so a frame
    // of the generation being flushed can land while the flush is still
    // awaiting the playback service.
    try await session.handleMedia(
        ChildFrame(
            audioSessionID: 7,
            frame: pcmFrame(generationID: 3, sequence: 1)
        )
    )

    await playback.releaseFlushes()
    try await flush.value

    #expect(await playback.frames.count == 1)
    #expect(
        await events.frames.last
            == ChildFrame(
                control: .playbackFlushed(
                    sessionID: 7,
                    generationID: 3,
                    operationID: 2
                )
            )
    )
}

@Test
func newerGenerationFrameArrivingDuringSuspendedFlushIsEnqueuedAfterward() async throws {
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
    try await session.handleMedia(
        ChildFrame(audioSessionID: 7, frame: pcmFrame(generationID: 3))
    )

    let flush = Task {
        try await session.handleControl(
            ChildFrame(
                control: .flushGeneration(
                    sessionID: 7,
                    generationID: 3,
                    operationID: 2
                )
            )
        )
    }
    await playback.waitUntilFlushStarted()

    // The next turn's audio can start streaming before the previous
    // generation's flush acknowledgement completes; the frame belongs to
    // a live generation and must survive the transient flush phase.
    try await session.handleMedia(
        ChildFrame(audioSessionID: 7, frame: pcmFrame(generationID: 4))
    )

    await playback.releaseFlushes()
    try await flush.value

    #expect(await playback.frames.map(\.generationID) == [3, 4])
    #expect(
        await events.frames.suffix(2)
            == [
                ChildFrame(
                    control: .playbackFlushed(
                        sessionID: 7,
                        generationID: 3,
                        operationID: 2
                    )
                ),
                ChildFrame(
                    control: .playbackAccepted(
                        sessionID: 7,
                        turnID: 4,
                        generationID: 4,
                        utteranceID: 1,
                        sequence: 0
                    )
                ),
            ]
    )
}

@Test
func staleParentFlushAfterLocalBargeInIsAcknowledgedWithoutFailure() async throws {
    let playback = RecordingPlaybackService()
    let events = RecordingEventSink()
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
    _ = try await session.observeBargeIn(
        isSpeech: true,
        frameMilliseconds: 100,
        atMilliseconds: 10
    )
    #expect(
        try await session.observeBargeIn(
            isSpeech: true,
            frameMilliseconds: 100,
            atMilliseconds: 110
        ) == true
    )
    try await session.handleMedia(
        ChildFrame(audioSessionID: 7, frame: pcmFrame(generationID: 6))
    )

    try await session.handleControl(
        ChildFrame(
            control: .flushGeneration(
                sessionID: 7,
                generationID: 5,
                operationID: 11
            )
        )
    )

    #expect(await playback.flushedGenerations == [5])
    #expect(
        await events.frames.last
            == ChildFrame(
                control: .playbackFlushed(
                    sessionID: 7,
                    generationID: 5,
                    operationID: 11
                )
            )
    )
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
    #expect(await events.frames.count == 2)
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
func controlArrivingDuringSuspendedFlushWaitsForTheFlushToSettle() async throws {
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

    // A runtime interrupt flush regularly races a local barge-in flush, so
    // a control landing mid-flush is normal operation, not a violation. It
    // must wait for the flush to settle instead of failing the session.
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

    // Give the racing control ample scheduler turns to enter handleControl
    // while the first flush still holds the transient phase.
    for _ in 0..<100 {
        await Task.yield()
    }
    await playback.releaseFlushes()
    try await firstFlush.value

    #expect(await secondFlush.value == nil)
    #expect(await secondDone.isComplete)
    #expect(
        await waitUntil {
            await playback.flushedGenerations == [3, 4]
        }
    )
    #expect(
        await events.frames.suffix(2)
            == [
                ChildFrame(
                    control: .playbackFlushed(
                        sessionID: 7,
                        generationID: 3,
                        operationID: 1
                    )
                ),
                ChildFrame(
                    control: .playbackFlushed(
                        sessionID: 7,
                        generationID: 4,
                        operationID: 2
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
            ChildFrame(control: .startCapture(sessionID: 7, operationID: 1))
        )
    }
    await audio.waitUntilStarted()

    let duplicateDone = CompletionFlag()
    let duplicateStart = Task { () -> SidecarSessionError? in
        do {
            try await session.handleControl(
                ChildFrame(control: .startCapture(sessionID: 7, operationID: 1))
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
    #expect(
        await events.frames
            == [
                ChildFrame(control: .ready(sessionID: 7)),
                ChildFrame(control: .captureStarted(sessionID: 7, operationID: 1)),
            ]
    )

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
            ChildFrame(control: .startCapture(sessionID: 7, operationID: 1))
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
            ChildFrame(control: .startCapture(sessionID: 7, operationID: 1))
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
            ChildFrame(control: .startCapture(sessionID: 7, operationID: 1))
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
        ChildFrame(control: .startCapture(sessionID: 7, operationID: 1))
    )
}

// MARK: - Turn audio

/// 25,600 samples at a quarter of full scale: 1.6 s, of which the closing
/// 600 ms is the final silence the snapshot drops.
private let turnSamples = ContiguousArray<Float>(repeating: 0.25, count: 25_600)
private let turnPCM = [Int16](repeating: 8_192, count: 16_000)

private func partial(_ segmentID: UInt64) -> RecognitionHypothesis {
    RecognitionHypothesis(segmentID: segmentID, text: "partial", engineFinal: false)
}

private func final(_ segmentID: UInt64) -> RecognitionHypothesis {
    RecognitionHypothesis(segmentID: segmentID, text: "final", engineFinal: true)
}

private func hypothesisFrame(
    _ hypothesis: RecognitionHypothesis,
    version: UInt16
) -> ChildFrame {
    ChildFrame(
        control: .transcriptHypothesis(sessionID: 7, hypothesis: hypothesis)
    ).atVersion(version)
}

private func turnAudioFrame(
    segmentID: UInt64,
    finalSegments: UInt16,
    playbackOverlap: Bool,
    samples: [Int16] = turnPCM
) -> ChildFrame {
    ChildFrame(
        turnAudio: TurnAudioFrame(
            sessionID: 7,
            segmentID: segmentID,
            finalSegments: finalSegments,
            playbackOverlap: playbackOverlap,
            samples: samples
        )
    )
}

private func capturingSession(
    version: UInt16,
    audioService: any SidecarAudioService = RecordingAudioService()
) async throws -> (SidecarSession, RecordingEventSink) {
    let events = RecordingEventSink()
    let session = SidecarSession(
        audioService: audioService,
        recognitionService: RecordingRecognitionService(),
        playbackService: RecordingPlaybackService(),
        eventSink: events
    )
    try await session.handleControl(
        ChildFrame(
            control: .startSession(
                sessionID: 7,
                speechStartMilliseconds: 200,
                finalSilenceMilliseconds: 600
            )
        ).atVersion(version)
    )
    try await session.handleControl(
        ChildFrame(control: .startCapture(sessionID: 7, operationID: 1))
            .atVersion(version)
    )
    return (session, events)
}

private func startPlayback(
    _ session: SidecarSession,
    generationID: UInt64 = 5,
    version: UInt16 = 2
) async throws -> PCMFrame {
    let frame = try pcmFrame(generationID: generationID)
    try await session.handleMedia(
        ChildFrame(audioSessionID: 7, frame: frame).atVersion(version)
    )
    return frame
}

private func speechWindow(
    _ session: SidecarSession,
    isSpeech: Bool = true,
    at atMilliseconds: UInt64
) async throws {
    try await session.observeBargeInFromRecognitionWorker(
        isSpeech: isSpeech,
        frameMilliseconds: 100,
        atMilliseconds: atMilliseconds
    )
}

private func turnAudioFrames(_ events: RecordingEventSink) async -> [ChildFrame] {
    await events.frames.filter { $0.turnAudio != nil }
}

@Test
func versionTwoSessionSendsTurnAudioAfterTheTurnsFinals() async throws {
    let (session, events) = try await capturingSession(version: 2)

    try await session.publishRecognitionHypothesisFromWorker(partial(1))
    try await session.publishRecognitionHypothesisFromWorker(final(1))
    try await session.publishTurnEndedFromRecognitionWorker(samples: turnSamples)
    // The next turn's first hypothesis restarts at segment 1.
    try await session.publishRecognitionHypothesisFromWorker(partial(1))

    #expect(
        await events.frames
            == [
                ChildFrame(control: .ready(sessionID: 7)).atVersion(2),
                ChildFrame(control: .captureStarted(sessionID: 7, operationID: 1))
                    .atVersion(2),
                hypothesisFrame(partial(1), version: 2),
                hypothesisFrame(final(1), version: 2),
                turnAudioFrame(segmentID: 1, finalSegments: 1, playbackOverlap: false),
                hypothesisFrame(partial(1), version: 2),
            ]
    )
}

@Test
func versionOneSessionNeverSendsTurnAudio() async throws {
    let (session, events) = try await capturingSession(version: 1)
    _ = try await startPlayback(session, version: 1)
    try await speechWindow(session, at: 10)

    try await session.publishRecognitionHypothesisFromWorker(final(1))
    try await session.publishTurnEndedFromRecognitionWorker(samples: turnSamples)
    try await session.publishRecognitionHypothesisFromWorker(final(1))
    try await session.publishTurnEndedFromRecognitionWorker(samples: turnSamples)

    let frames = await events.frames
    #expect(frames.allSatisfy { $0.version == 1 })
    #expect(
        frames
            == [
                ChildFrame(control: .ready(sessionID: 7)),
                ChildFrame(control: .captureStarted(sessionID: 7, operationID: 1)),
                ChildFrame(
                    control: .playbackAccepted(
                        sessionID: 7,
                        turnID: 5,
                        generationID: 5,
                        utteranceID: 1,
                        sequence: 0
                    )
                ),
                hypothesisFrame(final(1), version: 1),
                hypothesisFrame(final(1), version: 1),
            ]
    )
}

@Test
func aTurnWithoutAFinalSendsNoTurnAudio() async throws {
    let (session, events) = try await capturingSession(version: 2)

    try await session.publishRecognitionHypothesisFromWorker(partial(1))
    try await session.publishRecognitionHypothesisFromWorker(partial(2))
    try await session.publishTurnEndedFromRecognitionWorker(samples: turnSamples)
    #expect(await turnAudioFrames(events).isEmpty)

    // A turn whose buffer is all closing silence has an empty result.
    try await session.publishRecognitionHypothesisFromWorker(final(1))
    try await session.publishTurnEndedFromRecognitionWorker(
        samples: ContiguousArray(repeating: 0.25, count: 9_600)
    )
    #expect(await turnAudioFrames(events).isEmpty)

    // Neither turn leaks into the next one.
    try await session.publishRecognitionHypothesisFromWorker(final(1))
    try await session.publishTurnEndedFromRecognitionWorker(samples: turnSamples)
    #expect(
        await turnAudioFrames(events)
            == [turnAudioFrame(segmentID: 1, finalSegments: 1, playbackOverlap: false)]
    )
}

@Test
func aJoinedTurnReportsItsFinalSegmentCount() async throws {
    let (session, events) = try await capturingSession(version: 2)

    try await session.publishRecognitionHypothesisFromWorker(final(1))
    try await session.publishRecognitionHypothesisFromWorker(partial(2))
    try await session.publishRecognitionHypothesisFromWorker(final(2))
    // A repeated final of the same segment is still one segment.
    try await session.publishRecognitionHypothesisFromWorker(final(2))
    try await session.publishTurnEndedFromRecognitionWorker(samples: turnSamples)

    #expect(
        await events.frames.last
            == turnAudioFrame(segmentID: 2, finalSegments: 2, playbackOverlap: false)
    )
}

@Test
func playbackDuringSpeechMarksTheTurn() async throws {
    let (session, events) = try await capturingSession(version: 2)
    _ = try await startPlayback(session)
    // One speech window is below the barge-in threshold: playback continues.
    try await speechWindow(session, at: 10)

    try await session.publishRecognitionHypothesisFromWorker(final(1))
    try await session.publishTurnEndedFromRecognitionWorker(samples: turnSamples)

    #expect(
        await turnAudioFrames(events)
            == [turnAudioFrame(segmentID: 1, finalSegments: 1, playbackOverlap: true)]
    )
}

@Test
func aSpeechStartDuringPlaybackMarksTheTurn() async throws {
    let (session, events) = try await capturingSession(version: 2)
    _ = try await startPlayback(session)

    try await session.publishVoiceActivityFromRecognitionWorker(
        .speechStarted(atMilliseconds: 10)
    )
    try await session.publishRecognitionHypothesisFromWorker(final(1))
    try await session.publishTurnEndedFromRecognitionWorker(samples: turnSamples)

    #expect(
        await turnAudioFrames(events)
            == [turnAudioFrame(segmentID: 1, finalSegments: 1, playbackOverlap: true)]
    )
}

@Test
func aBargeInMarksTheTurn() async throws {
    let (session, events) = try await capturingSession(version: 2)
    _ = try await startPlayback(session)
    try await speechWindow(session, at: 10)
    try await speechWindow(session, at: 110)
    #expect(
        await events.frames.last
            == ChildFrame(
                control: .voiceActivity(
                    sessionID: 7,
                    activity: .speechStarted(atMilliseconds: 110)
                )
            ).atVersion(2)
    )

    try await session.publishRecognitionHypothesisFromWorker(final(1))
    try await session.publishTurnEndedFromRecognitionWorker(samples: turnSamples)

    #expect(
        await turnAudioFrames(events)
            == [turnAudioFrame(segmentID: 1, finalSegments: 1, playbackOverlap: true)]
    )
}

@Test
func playbackThatEndedBeforeSpeechDoesNotMarkTheTurn() async throws {
    let (session, events) = try await capturingSession(version: 2)
    let frame = try await startPlayback(session)
    // Silence during playback is not speech over it.
    try await speechWindow(session, isSpeech: false, at: 10)
    try await session.playbackRendered(frame.identity)

    try await speechWindow(session, at: 110)
    try await speechWindow(session, at: 210)
    try await session.publishVoiceActivityFromRecognitionWorker(
        .speechStarted(atMilliseconds: 210)
    )
    try await session.publishRecognitionHypothesisFromWorker(final(1))
    try await session.publishVoiceActivityFromRecognitionWorker(
        .speechEnded(atMilliseconds: 900)
    )
    try await session.publishTurnEndedFromRecognitionWorker(samples: turnSamples)

    #expect(
        await turnAudioFrames(events)
            == [turnAudioFrame(segmentID: 1, finalSegments: 1, playbackOverlap: false)]
    )
}

@Test
func theNextTurnStartsUnmarked() async throws {
    let (session, events) = try await capturingSession(version: 2)
    let frame = try await startPlayback(session)
    try await speechWindow(session, at: 10)
    try await session.publishVoiceActivityFromRecognitionWorker(
        .speechStarted(atMilliseconds: 10)
    )
    try await session.publishRecognitionHypothesisFromWorker(final(1))
    try await session.publishVoiceActivityFromRecognitionWorker(
        .speechEnded(atMilliseconds: 900)
    )
    try await session.playbackRendered(frame.identity)
    try await session.publishTurnEndedFromRecognitionWorker(samples: turnSamples)

    try await speechWindow(session, at: 2_000)
    try await session.publishVoiceActivityFromRecognitionWorker(
        .speechStarted(atMilliseconds: 2_000)
    )
    try await session.publishRecognitionHypothesisFromWorker(final(1))
    try await session.publishVoiceActivityFromRecognitionWorker(
        .speechEnded(atMilliseconds: 2_900)
    )
    try await session.publishTurnEndedFromRecognitionWorker(samples: turnSamples)

    #expect(
        await turnAudioFrames(events)
            == [
                turnAudioFrame(segmentID: 1, finalSegments: 1, playbackOverlap: true),
                turnAudioFrame(segmentID: 1, finalSegments: 1, playbackOverlap: false),
            ]
    )
}

@Test
func speechOverPlaybackAfterATurnsSpeechEndedAlsoMarksTheNextTurn() async throws {
    // The backend ends a turn some time after its speech ended. Speech that
    // starts in between, over the companion's playback, can reach the
    // session before the turn end does; it belongs to the next turn.
    let (session, events) = try await capturingSession(version: 2)
    try await session.publishVoiceActivityFromRecognitionWorker(
        .speechStarted(atMilliseconds: 10)
    )
    try await session.publishRecognitionHypothesisFromWorker(final(1))
    try await session.publishVoiceActivityFromRecognitionWorker(
        .speechEnded(atMilliseconds: 900)
    )
    let frame = try await startPlayback(session)
    try await speechWindow(session, at: 1_000)
    try await session.playbackRendered(frame.identity)
    try await session.publishTurnEndedFromRecognitionWorker(samples: turnSamples)

    try await session.publishVoiceActivityFromRecognitionWorker(
        .speechStarted(atMilliseconds: 1_100)
    )
    try await session.publishRecognitionHypothesisFromWorker(final(1))
    try await session.publishVoiceActivityFromRecognitionWorker(
        .speechEnded(atMilliseconds: 2_000)
    )
    try await session.publishTurnEndedFromRecognitionWorker(samples: turnSamples)

    #expect(
        await turnAudioFrames(events)
            == [
                turnAudioFrame(segmentID: 1, finalSegments: 1, playbackOverlap: true),
                turnAudioFrame(segmentID: 1, finalSegments: 1, playbackOverlap: true),
            ]
    )
}

@Test
func aCaptureDiscontinuityDropsTheTurn() async throws {
    let (session, events) = try await capturingSession(version: 2)
    let frame = try await startPlayback(session)
    try await speechWindow(session, at: 10)
    try await session.publishRecognitionHypothesisFromWorker(final(1))
    try await session.publishRecognitionHypothesisFromWorker(final(2))
    try await session.playbackRendered(frame.identity)

    try await session.observeCaptureDiscontinuityFromRecognitionWorker(
        atMilliseconds: 500
    )
    #expect(await turnAudioFrames(events).isEmpty)
    #expect(
        await events.frames.last
            == ChildFrame(
                control: .voiceActivity(
                    sessionID: 7,
                    activity: .captureDiscontinuity(atMilliseconds: 500)
                )
            ).atVersion(2)
    )

    // Nothing of the dropped turn — its finals or its overlap — carries on.
    try await session.publishRecognitionHypothesisFromWorker(final(1))
    try await session.publishTurnEndedFromRecognitionWorker(samples: turnSamples)
    #expect(
        await turnAudioFrames(events)
            == [turnAudioFrame(segmentID: 1, finalSegments: 1, playbackOverlap: false)]
    )
}

@Test
func everyFrameCarriesTheSessionsVersion() async throws {
    let (session, events) = try await capturingSession(
        version: 2,
        audioService: RecordingDeviceAudioService(
            status: AudioDeviceStatus(inputLabel: "Microphone", outputLabel: "Speakers")
        )
    )
    let frame = try await startPlayback(session)
    try await session.publishVoiceActivityFromRecognitionWorker(
        .speechStarted(atMilliseconds: 10)
    )
    try await session.publishRecognitionHypothesisFromWorker(final(1))
    try await session.publishTurnEndedFromRecognitionWorker(samples: turnSamples)
    try await session.playbackRendered(frame.identity)
    _ = try await startPlayback(session, generationID: 6)
    try await session.handleControl(
        ChildFrame(
            control: .flushGeneration(sessionID: 7, generationID: 6, operationID: 2)
        ).atVersion(2)
    )
    try await session.recoverFromRecognitionFailure(
        SidecarServiceFailure(stage: .speechRecognizer, code: .recognitionFailed),
        fallbackSessionID: 7
    )
    try await session.handleControl(
        ChildFrame(control: .pauseCapture(sessionID: 7, operationID: 3)).atVersion(2)
    )
    try await session.handleControl(
        ChildFrame(control: .resumeCapture(sessionID: 7, operationID: 4)).atVersion(2)
    )
    try await session.handleControl(
        ChildFrame(control: .shutdown(sessionID: 7)).atVersion(2)
    )

    let frames = await events.frames
    #expect(
        Set(frames.map(\.kind))
            == [
                .ready, .audioDeviceStatus, .captureStarted, .playbackAccepted,
                .voiceActivity, .transcriptHypothesis, .turnAudio,
                .playbackRendered, .playbackFlushed, .failure, .capturePaused,
                .captureResumed, .shutdownComplete,
            ]
    )
    #expect(frames.allSatisfy { $0.version == 2 })
}

@Test(arguments: [(UInt16(2), UInt16(1)), (UInt16(1), UInt16(2))])
func aFrameAtAnotherVersionFailsTheSession(
    sessionVersion: UInt16,
    frameVersion: UInt16
) async throws {
    let malformed = ChildFrame(
        control: .failure(sessionID: 7, stage: .voiceSidecar, code: .malformedFrame)
    ).atVersion(sessionVersion)

    let (controlSession, controlEvents) = try await capturingSession(
        version: sessionVersion
    )
    await #expect(throws: (any Error).self) {
        try await controlSession.handleControl(
            ChildFrame(control: .pauseCapture(sessionID: 7, operationID: 2))
                .atVersion(frameVersion)
        )
    }
    #expect(await controlEvents.frames.last == malformed)
    #expect(await controlEvents.frames.allSatisfy { $0.version == sessionVersion })

    let (mediaSession, mediaEvents) = try await capturingSession(
        version: sessionVersion
    )
    await #expect(throws: (any Error).self) {
        try await mediaSession.handleMedia(
            ChildFrame(audioSessionID: 7, frame: try pcmFrame(generationID: 5))
                .atVersion(frameVersion)
        )
    }
    #expect(await mediaEvents.frames.last == malformed)
    #expect(await mediaEvents.frames.allSatisfy { $0.version == sessionVersion })
}

@Test
func aSessionAnswersInTheVersionOfItsStartSession() async throws {
    // A failure before any StartSession has no session version yet.
    let unstarted = RecordingEventSink()
    let unstartedSession = SidecarSession(
        audioService: RecordingAudioService(),
        recognitionService: RecordingRecognitionService(),
        playbackService: RecordingPlaybackService(),
        eventSink: unstarted
    )
    await #expect(throws: (any Error).self) {
        try await unstartedSession.handleControl(
            ChildFrame(control: .startCapture(sessionID: 7, operationID: 1))
                .atVersion(2)
        )
    }
    #expect(
        await unstarted.frames
            == [
                ChildFrame(
                    control: .failure(sessionID: 7, stage: .voiceSidecar, code: .invalidState)
                )
            ]
    )

    // A StartSession the sidecar refuses is still answered in its version.
    let refused = RecordingEventSink()
    let refusedSession = SidecarSession(
        audioService: RecordingAudioService(),
        recognitionService: RecordingRecognitionService(),
        playbackService: RecordingPlaybackService(),
        eventSink: refused
    )
    await #expect(throws: (any Error).self) {
        try await refusedSession.handleControl(
            ChildFrame(
                control: .startSession(
                    sessionID: 7,
                    speechStartMilliseconds: 50,
                    finalSilenceMilliseconds: 600
                )
            ).atVersion(2)
        )
    }
    #expect(
        await refused.frames
            == [
                ChildFrame(
                    control: .failure(sessionID: 7, stage: .voiceSidecar, code: .invalidState)
                ).atVersion(2)
            ]
    )
}
