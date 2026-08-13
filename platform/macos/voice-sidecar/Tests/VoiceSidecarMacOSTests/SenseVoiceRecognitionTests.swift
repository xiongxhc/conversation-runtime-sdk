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
