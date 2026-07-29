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
    let audio = RecordingAudioService()
    let recognition = RecordingRecognitionService()
    let session = SidecarSession(
        audioService: audio,
        recognitionService: recognition,
        playbackService: RecordingPlaybackService(),
        eventSink: RecordingEventSink()
    )

    try await startSession(session)
    #expect(await audio.configurations.isEmpty)
    #expect(await recognition.configurations.isEmpty)

    try await session.handleControl(
        ChildFrame(control: .startCapture(sessionID: 7))
    )

    let expected = SidecarConfiguration(
        sessionID: 7,
        speechStartMilliseconds: 200,
        finalSilenceMilliseconds: 600
    )
    #expect(await audio.configurations == [expected])
    #expect(await recognition.configurations == [expected])
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
                ),
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

private func startSession(_ session: SidecarSession) async throws {
    try await session.handleControl(
        ChildFrame(
            control: .startSession(
                sessionID: 7,
                speechStartMilliseconds: 200,
                finalSilenceMilliseconds: 600
            )
        )
    )
}

private func startAndCapture(_ session: SidecarSession) async throws {
    try await startSession(session)
    try await session.handleControl(
        ChildFrame(control: .startCapture(sessionID: 7))
    )
}
