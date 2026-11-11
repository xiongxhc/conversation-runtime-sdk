import Foundation
import Testing

@testable import VoiceSidecarCore
@testable import VoiceSidecarMacOS

// A version 2 session fed the way a recognition backend feeds it: every
// 100 ms window goes into the turn buffer, through the speech gate and to the
// session, and the turn ends with the buffer's own turn end. Only the
// backend's model and clock are replaced.

private actor FrameSink: SidecarEventSink {
    private(set) var frames: [ChildFrame] = []

    func send(_ frame: ChildFrame) async throws {
        frames.append(frame)
    }
}

private struct QuietAudio: SidecarAudioService {
    func start(configuration _: SidecarConfiguration) async throws {}
    func pauseCapture() async throws {}
    func resumeCapture() async throws {}
    func stop() async {}
}

private struct QuietRecognition: SidecarRecognitionService {
    func prepare(configuration _: SidecarConfiguration) async throws {}
    func start(configuration _: SidecarConfiguration) async throws {}
    func stop() async {}
}

private struct QuietPlayback: SidecarPlaybackService {
    func enqueue(_: PCMFrame) async throws {}
    func flush(throughGenerationID _: UInt64) async throws {}
    func stop() async {}
}

private let configuration = SidecarConfiguration(
    sessionID: 7,
    speechStartMilliseconds: 200,
    finalSilenceMilliseconds: 600,
    collectsTurnAudio: true
)

/// The speech start is published two windows (the 200 ms threshold) after
/// the speech began, and the sent audio starts the 300 ms pre-roll before it.
private let preRollSamples = 4_800

private final class BackendDriver {
    let session: SidecarSession
    let sink = FrameSink()
    let turn = TurnAudioProcessor()
    var gate = RecognitionSpeechGate(thresholdMilliseconds: 200)
    var clock: UInt64 = 0
    var segmentID: UInt64 = 0

    init() async throws {
        session = SidecarSession(
            audioService: QuietAudio(),
            recognitionService: QuietRecognition(),
            playbackService: QuietPlayback(),
            eventSink: sink
        )
        try await session.handleControl(
            ChildFrame(
                control: .startSession(
                    sessionID: 7,
                    speechStartMilliseconds: 200,
                    finalSilenceMilliseconds: 600
                )
            ).atVersion(2)
        )
        try await session.handleControl(
            ChildFrame(control: .startCapture(sessionID: 7, operationID: 1))
                .atVersion(2)
        )
        try turn.startRecordingLive(inputDeviceID: nil, callback: nil)
    }

    func windows(_ count: Int, speech: Bool) async throws {
        for _ in 0..<count {
            clock += 100
            turn.append([Float](repeating: speech ? 0.25 : 0, count: 1_600))
            try await session.observeBargeInFromRecognitionWorker(
                isSpeech: speech,
                frameMilliseconds: 100,
                atMilliseconds: clock
            )
            let transition = gate.observe(isSpeech: speech)
            turn.observe(transition, windowEndingAt: turn.capturePosition)
            switch transition {
            case .started:
                try await session.publishVoiceActivityFromRecognitionWorker(
                    .speechStarted(atMilliseconds: clock)
                )
            case .ended:
                try await session.publishVoiceActivityFromRecognitionWorker(
                    .speechEnded(atMilliseconds: clock)
                )
            case .continued, .none:
                break
            }
        }
    }

    func final() async throws {
        segmentID += 1
        try await session.publishRecognitionHypothesisFromWorker(
            RecognitionHypothesis(segmentID: segmentID, text: "final", engineFinal: true)
        )
    }

    /// The final-silence reset, as SenseVoice runs it.
    func endTurn() async throws {
        try await session.publishTurnEndedFromRecognitionWorker(
            turn.turnEnd(
                configuration: configuration,
                atMilliseconds: clock,
                discontinuities: 0
            )
        )
        turn.beginTransition()
        try turn.startRecordingLive(inputDeviceID: nil, callback: nil)
        segmentID = 0
    }

    func startPlayback(generationID: UInt64) async throws -> PCMFrame {
        let frame = try PCMFrame(
            turnID: generationID,
            generationID: generationID,
            utteranceID: 1,
            sequence: 0,
            format: PCMFormat(
                sampleRateHz: 24_000,
                channels: 1,
                sampleFormat: .signed16LittleEndian
            ),
            bytes: Data(repeating: 0, count: 960)
        )
        try await session.handleMedia(
            ChildFrame(audioSessionID: 7, frame: frame).atVersion(2)
        )
        return frame
    }

    func turnAudio() async -> [TurnAudioFrame] {
        await sink.frames.compactMap(\.turnAudio)
    }
}

@Test
func aShortTurnAfterALongSilenceSendsOnlyTheSpeechInTheBuffer() async throws {
    let driver = try await BackendDriver()
    try await driver.windows(150, speech: false)
    try await driver.windows(15, speech: true)
    try await driver.final()
    try await driver.windows(6, speech: false)
    try await driver.endTurn()

    let audio = try #require(await driver.turnAudio().first)
    // The pre-roll, 1.5 s of speech and the window that ended it; not the
    // 15 s of silence before it.
    #expect(audio.samples.count == preRollSamples + 24_000 + 1_600)
    #expect(audio.samples.suffix(1_600 + 24_000).prefix(24_000).allSatisfy { $0 == 8_192 })
    #expect(!audio.playbackOverlap)
}

@Test
func playbackInTheGapThatEndedBeforeThePersonSpeaksLeavesTheTurnUnmarked() async throws {
    let driver = try await BackendDriver()
    // A first turn, then the companion's reply in the gap.
    try await driver.windows(10, speech: true)
    try await driver.final()
    try await driver.windows(6, speech: false)
    try await driver.endTurn()
    let reply = try await driver.startPlayback(generationID: 5)
    try await driver.windows(60, speech: false)
    try await driver.session.playbackRendered(reply.identity)
    try await driver.windows(20, speech: false)

    try await driver.windows(15, speech: true)
    try await driver.final()
    try await driver.windows(6, speech: false)
    try await driver.endTurn()

    let audio = await driver.turnAudio()
    #expect(audio.count == 2)
    #expect(audio.last?.samples.count == preRollSamples + 24_000 + 1_600)
    #expect(audio.last?.playbackOverlap == false)
}

@Test
func playbackInAPauseInsideTheTurnMarksIt() async throws {
    let driver = try await BackendDriver()
    try await driver.windows(10, speech: false)
    try await driver.windows(10, speech: true)
    try await driver.windows(1, speech: false)
    let reply = try await driver.startPlayback(generationID: 5)
    try await driver.windows(2, speech: false)
    try await driver.session.playbackRendered(reply.identity)
    try await driver.windows(1, speech: false)
    try await driver.windows(10, speech: true)
    try await driver.final()
    try await driver.windows(6, speech: false)
    try await driver.endTurn()

    let audio = try #require(await driver.turnAudio().first)
    // Both stretches and the pause between them.
    #expect(audio.samples.count == preRollSamples + (10 + 4 + 10 + 1) * 1_600)
    #expect(audio.playbackOverlap)
}

@Test
func aTurnWithAFinalButNoSpeechStartSendsNothing() async throws {
    let driver = try await BackendDriver()
    // Speech shorter than the threshold never starts the gate.
    try await driver.windows(30, speech: false)
    try await driver.windows(1, speech: true)
    try await driver.final()
    try await driver.windows(6, speech: false)
    try await driver.endTurn()

    #expect(await driver.turnAudio().isEmpty)
}
