import AVFoundation
import Foundation
import Testing

@testable import VoiceSidecarCore
@testable import VoiceSidecarMacOS

private let senseVoiceModelDirectory = FileManager.default
    .homeDirectoryForCurrentUser
    .appendingPathComponent(
        ".local/share/conversation-runtime/models/sensevoice/"
            + "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17",
        isDirectory: true
    )

private func senseVoiceModelAvailable() -> Bool {
    FileManager.default.fileExists(
        atPath: senseVoiceModelDirectory
            .appendingPathComponent("model.int8.onnx").path
    )
        && FileManager.default.fileExists(
            atPath: senseVoiceModelDirectory
                .appendingPathComponent("tokens.txt").path
        )
}

private func loadTestWavSamples(_ name: String) throws -> [Float] {
    let url = senseVoiceModelDirectory
        .appendingPathComponent("test_wavs", isDirectory: true)
        .appendingPathComponent(name, isDirectory: false)
    let file = try AVAudioFile(forReading: url)
    let format = file.processingFormat
    try #require(format.sampleRate == 16_000)
    try #require(format.channelCount == 1)
    let buffer = try #require(
        AVAudioPCMBuffer(
            pcmFormat: format,
            frameCapacity: AVAudioFrameCount(file.length)
        )
    )
    try file.read(into: buffer)
    let channel = try #require(buffer.floatChannelData?[0])
    return Array(
        UnsafeBufferPointer(
            start: channel,
            count: Int(buffer.frameLength)
        )
    )
}

@Test
func senseVoicePrepareRejectsMissingModelDirectory() async {
    let recognition = SenseVoiceRecognition(
        modelPath: "/definitely/missing/sensevoice-model",
        audioProcessor: VoiceProcessingAudioProcessor(
            engine: VoiceProcessingEngine()
        )
    )
    await #expect(
        throws: SidecarServiceFailure(
            stage: .speechRecognizer,
            code: .recognitionFailed
        )
    ) {
        try await recognition.prepare(
            configuration: SidecarConfiguration(
                sessionID: 1,
                speechStartMilliseconds: 200,
                finalSilenceMilliseconds: 600
            )
        )
    }
}

@Test
func senseVoicePrepareRejectsUnsupportedLanguage() async {
    let recognition = SenseVoiceRecognition(
        modelPath: "/definitely/missing/sensevoice-model",
        audioProcessor: VoiceProcessingAudioProcessor(
            engine: VoiceProcessingEngine()
        ),
        language: "de"
    )
    await #expect(
        throws: SidecarServiceFailure(
            stage: .speechRecognizer,
            code: .recognitionFailed
        )
    ) {
        try await recognition.prepare(
            configuration: SidecarConfiguration(
                sessionID: 1,
                speechStartMilliseconds: 200,
                finalSilenceMilliseconds: 600
            )
        )
    }
}

@Test
func senseVoiceSegmentTrackerNumbersPartialsAndFinals() {
    var tracker = SenseVoiceSegmentTracker()

    let firstPartial = tracker.partialHypothesis(text: "你好")
    #expect(
        firstPartial
            == RecognitionHypothesis(
                segmentID: 1,
                text: "你好",
                engineFinal: false
            )
    )
    #expect(tracker.partialHypothesis(text: "你好") == nil)

    let final = tracker.finalHypothesis(
        text: "你好世界",
        decodedThrough: 32_000
    )
    #expect(
        final
            == RecognitionHypothesis(
                segmentID: 1,
                text: "你好世界",
                engineFinal: true
            )
    )
    #expect(tracker.finalizedSampleCount == 32_000)

    let nextPartial = tracker.partialHypothesis(text: "hello")
    #expect(
        nextPartial
            == RecognitionHypothesis(
                segmentID: 2,
                text: "hello",
                engineFinal: false
            )
    )
}

@Test
func senseVoiceSegmentTrackerSkipsUnrecognizableText() {
    var tracker = SenseVoiceSegmentTracker()

    #expect(tracker.partialHypothesis(text: "  ") == nil)
    #expect(tracker.partialHypothesis(text: "…") == nil)
    #expect(tracker.finalHypothesis(text: "", decodedThrough: 8_000) == nil)
    #expect(tracker.finalizedSampleCount == 8_000)
    #expect(
        tracker.finalHypothesis(text: "ok", decodedThrough: 16_000)?
            .segmentID == 1
    )
}

@Test
func senseVoiceSegmentTrackerResetsForNewTurn() {
    var tracker = SenseVoiceSegmentTracker()
    _ = tracker.finalHypothesis(text: "first", decodedThrough: 16_000)
    #expect(tracker.segmentID == 2)

    tracker.resetForNewTurn()

    #expect(tracker.segmentID == 1)
    #expect(tracker.finalizedSampleCount == 0)
    #expect(
        tracker.partialHypothesis(text: "first") != nil,
        "reset must clear partial de-duplication state"
    )
}

@Test
func senseVoiceDecodesBundledChineseAndEnglishClips() throws {
    guard senseVoiceModelAvailable() else {
        return
    }
    let engine = SenseVoiceEngine(
        modelFilePath: senseVoiceModelDirectory
            .appendingPathComponent("model.int8.onnx").path,
        tokensFilePath: senseVoiceModelDirectory
            .appendingPathComponent("tokens.txt").path,
        language: "auto"
    )

    let chinese = engine.transcribe(try loadTestWavSamples("zh.wav"))
    #expect(!chinese.isEmpty)
    #expect(
        chinese.unicodeScalars.contains {
            (0x4E00...0x9FFF).contains($0.value)
        },
        "expected CJK content, got: \(chinese)"
    )

    let english = engine.transcribe(try loadTestWavSamples("en.wav"))
    #expect(!english.isEmpty)
    #expect(
        english.range(
            of: "[A-Za-z]{2,}",
            options: .regularExpression
        ) != nil,
        "expected latin words, got: \(english)"
    )
}

private actor TurnAudioTestSink: SidecarEventSink {
    private(set) var frames: [ChildFrame] = []

    var hasTurnAudio: Bool {
        frames.contains { $0.turnAudio != nil }
    }

    func send(_ frame: ChildFrame) async throws {
        frames.append(frame)
    }
}

private actor TurnEndRecorder {
    private(set) var sampleCounts: [Int] = []

    func record(_ event: WhisperKitRecognitionEvent) {
        if case .turnEnded(let end) = event {
            sampleCounts.append(end.samples.count)
        }
    }
}

private struct IdleAudioService: SidecarAudioService {
    func start(configuration _: SidecarConfiguration) async throws {}
    func pauseCapture() async throws {}
    func resumeCapture() async throws {}
    func stop() async {}
}

private struct IdlePlaybackService: SidecarPlaybackService {
    func enqueue(_: PCMFrame) async throws {}
    func flush(throughGenerationID _: UInt64) async throws {}
    func stop() async {}
}

private func captureBuffer(_ samples: ArraySlice<Float>) throws -> AVAudioPCMBuffer {
    let format = try #require(
        AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: 16_000,
            channels: 1,
            interleaved: false
        )
    )
    let buffer = try #require(
        AVAudioPCMBuffer(
            pcmFormat: format,
            frameCapacity: AVAudioFrameCount(samples.count)
        )
    )
    buffer.frameLength = AVAudioFrameCount(samples.count)
    let channel = try #require(buffer.floatChannelData?[0])
    for (offset, sample) in samples.enumerated() {
        channel[offset] = sample
    }
    return buffer
}

@Test(.enabled(if: senseVoiceModelAvailable()), arguments: [UInt16(1), UInt16(2)])
func finalSilencePublishesTheTurnBeforeItsBufferIsCleared(version: UInt16) async throws {
    let audioProcessor = VoiceProcessingAudioProcessor(
        engine: VoiceProcessingEngine()
    )
    let recognition = SenseVoiceRecognition(
        modelPath: senseVoiceModelDirectory.path,
        audioProcessor: audioProcessor
    )
    let sink = TurnAudioTestSink()
    let session = SidecarSession(
        audioService: IdleAudioService(),
        recognitionService: recognition,
        playbackService: IdlePlaybackService(),
        eventSink: sink
    )
    let publisher = SidecarRecognitionEventPublisher(session: session)
    let turnEnds = TurnEndRecorder()
    await recognition.setEventHandler { event in
        await turnEnds.record(event)
        return try await publisher.publish(event)
    }
    try await session.handleControl(
        ChildFrame(
            control: .startSession(
                sessionID: 7,
                speechStartMilliseconds: 200,
                finalSilenceMilliseconds: 1_000
            )
        ).atVersion(version)
    )
    try await session.handleControl(
        ChildFrame(control: .startCapture(sessionID: 7, operationID: 1))
            .atVersion(version)
    )

    // The clip goes in five times faster than real time, so pauses inside
    // it stay far shorter than the final silence and the clip is one turn.
    // It stops after its first 4.4 s, which end in speech: past that, a lone
    // window the VAD takes for speech follows the speech end and cancels the
    // final-silence reset without a later speech end to schedule another.
    // Three times over, 13.2 s, is longer than the 10 s a turn keeps.
    let clip = Array(try loadTestWavSamples("zh.wav").prefix(70_400))
    let speech = clip + clip + clip
    for start in stride(from: 0, to: speech.count, by: 1_600) {
        audioProcessor.consumeCaptureEvent(
            .buffer(try captureBuffer(speech[start..<min(start + 1_600, speech.count)]))
        )
        try await Task.sleep(for: .milliseconds(20))
    }
    // Silence twice as fast as real time, until the turn ends. A version 1
    // session never sends turn audio, so it gets 3 s: three final silences.
    let silence = [Float](repeating: 0, count: 1_600)
    let deadline = ContinuousClock.now.advanced(
        by: version == 2 ? .seconds(30) : .seconds(3)
    )
    while await !sink.hasTurnAudio, ContinuousClock.now < deadline {
        audioProcessor.consumeCaptureEvent(
            .buffer(try captureBuffer(silence[...]))
        )
        try await Task.sleep(for: .milliseconds(50))
    }
    try await session.handleControl(
        ChildFrame(control: .shutdown(sessionID: 7)).atVersion(version)
    )

    let frames = await sink.frames
    guard version == 2 else {
        // A version 1 session's backend copies and publishes no turn audio.
        #expect(await turnEnds.sampleCounts.isEmpty)
        #expect(!frames.contains { $0.turnAudio != nil })
        #expect(frames.contains {
            guard case .transcriptHypothesis(_, let hypothesis) = $0.control else {
                return false
            }
            return hypothesis.engineFinal
        })
        #expect(frames.allSatisfy { $0.version == 1 })
        return
    }
    let turnAudioIndex = try #require(frames.firstIndex { $0.turnAudio != nil })
    let turnAudio = try #require(frames[turnAudioIndex].turnAudio)
    let finals = frames[..<turnAudioIndex].compactMap { frame -> UInt64? in
        guard case .transcriptHypothesis(_, let hypothesis) = frame.control,
            hypothesis.engineFinal
        else {
            return nil
        }
        return hypothesis.segmentID
    }
    // Every final of the turn was written before its audio.
    #expect(!finals.isEmpty)
    #expect(turnAudio.segmentID == finals.last)
    #expect(turnAudio.finalSegments == UInt16(Set(finals).count))
    #expect(!turnAudio.playbackOverlap)
    // The audio is the turn's last 10 s, not the pre-roll left after the
    // reset.
    #expect(turnAudio.samples.count == 160_000)
    #expect(turnAudio.samples.contains { abs(Int($0)) > 1_000 })
    #expect(frames.filter { $0.turnAudio != nil }.count == 1)
    #expect(frames.allSatisfy { $0.version == 2 })
    // The backend handed over only the turn's speech, already cut to 10 s.
    #expect(await turnEnds.sampleCounts == [160_000])
}

@Test(.enabled(if: senseVoiceModelAvailable()))
func senseVoiceSendsOnlyTheSpeechOfAShortTurnAfterALongSilence() async throws {
    let audioProcessor = VoiceProcessingAudioProcessor(
        engine: VoiceProcessingEngine()
    )
    let recognition = SenseVoiceRecognition(
        modelPath: senseVoiceModelDirectory.path,
        audioProcessor: audioProcessor
    )
    let sink = TurnAudioTestSink()
    let session = SidecarSession(
        audioService: IdleAudioService(),
        recognitionService: recognition,
        playbackService: IdlePlaybackService(),
        eventSink: sink
    )
    let publisher = SidecarRecognitionEventPublisher(session: session)
    await recognition.setEventHandler { event in
        try await publisher.publish(event)
    }
    try await session.handleControl(
        ChildFrame(
            control: .startSession(
                sessionID: 7,
                speechStartMilliseconds: 200,
                finalSilenceMilliseconds: 1_000
            )
        ).atVersion(2)
    )
    try await session.handleControl(
        ChildFrame(control: .startCapture(sessionID: 7, operationID: 1))
            .atVersion(2)
    )

    // 12 s of silence, five times faster than real time: no speech, so no
    // reset, and the turn buffer holds all of it.
    let silence = [Float](repeating: 0, count: 1_600)
    for _ in 0..<120 {
        audioProcessor.consumeCaptureEvent(.buffer(try captureBuffer(silence[...])))
        try await Task.sleep(for: .milliseconds(20))
    }
    // Then 4.4 s of speech, as in the test above.
    let speech = Array(try loadTestWavSamples("zh.wav").prefix(70_400))
    for start in stride(from: 0, to: speech.count, by: 1_600) {
        audioProcessor.consumeCaptureEvent(
            .buffer(try captureBuffer(speech[start..<min(start + 1_600, speech.count)]))
        )
        try await Task.sleep(for: .milliseconds(20))
    }
    let deadline = ContinuousClock.now.advanced(by: .seconds(30))
    while await !sink.hasTurnAudio, ContinuousClock.now < deadline {
        audioProcessor.consumeCaptureEvent(.buffer(try captureBuffer(silence[...])))
        try await Task.sleep(for: .milliseconds(50))
    }
    try await session.handleControl(
        ChildFrame(control: .shutdown(sessionID: 7)).atVersion(2)
    )

    let turnAudio = try #require(await sink.frames.compactMap(\.turnAudio).first)
    // The speech and its lead (the 200 ms speech-start threshold and the
    // 300 ms pre-roll), not the silence before it: well under 10 s.
    #expect(turnAudio.samples.count > 32_000)
    #expect(turnAudio.samples.count <= 70_400 + 8_000 + 16_000)
    #expect(turnAudio.samples.contains { abs(Int($0)) > 1_000 })
    #expect(!turnAudio.playbackOverlap)
}
