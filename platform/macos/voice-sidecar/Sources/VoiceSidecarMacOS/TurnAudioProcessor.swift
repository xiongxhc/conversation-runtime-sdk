@preconcurrency import AVFoundation
import CoreML
import Foundation
import VoiceSidecarCore
@preconcurrency import WhisperKit

enum TurnAudioProcessorError: Error, Equatable {
    case capacityExceeded
}

final class TurnAudioProcessor: AudioProcessing, @unchecked Sendable {
    typealias FailureHandler = @Sendable () -> Void

    private struct State {
        var preRoll: ContiguousArray<Float> = []
        var transitionSamples: ContiguousArray<Float> = []
        var audioSamples: ContiguousArray<Float> = []
        var energy: [(relative: Float, average: Float)] = []
        var relativeEnergyWindow = 20
        var callback: (([Float]) -> Void)?
        var failureHandler: FailureHandler?
        var isRecording = false
        var isPaused = true
        var isTransitioning = false
        var hasFailed = false
        /// Samples appended since the processor was created or reset; the
        /// clock the turn's speech marks are on.
        var capturePosition: UInt64 = 0
        /// The capture position of `audioSamples`' first sample.
        var audioStartPosition: UInt64 = 0
        /// Capture skipped while the turn buffer held samples, so positions
        /// past the gap no longer map onto it.
        var audioHasGap = false
        /// The voice window that published the turn's first speech start.
        var firstSpeechStart: UInt64?
        /// The voice window that published the turn's last speech end.
        var lastSpeechEnd: UInt64?
    }

    private let preRollSampleLimit: Int
    private let maxTurnSampleLimit: Int
    private let maxTransitionSampleLimit: Int
    private let stateLock = NSLock()
    private var state = State()

    var audioSamples: ContiguousArray<Float> {
        stateLock.withLock {
            state.audioSamples
        }
    }

    var relativeEnergy: [Float] {
        stateLock.withLock {
            state.energy.map(\.relative)
        }
    }

    var relativeEnergyWindow: Int {
        get {
            stateLock.withLock {
                state.relativeEnergyWindow
            }
        }
        set {
            stateLock.withLock {
                state.relativeEnergyWindow = max(1, newValue)
            }
        }
    }

    init(
        preRollSamples: Int = 4_800,
        maxTurnSamples: Int = 9_600_000,
        maxTransitionSamples: Int = 480_000
    ) {
        let maxTurnSampleLimit = max(1, maxTurnSamples)
        let preRollSampleLimit = min(
            max(0, preRollSamples),
            maxTurnSampleLimit
        )
        self.preRollSampleLimit = preRollSampleLimit
        self.maxTurnSampleLimit = maxTurnSampleLimit
        maxTransitionSampleLimit = min(
            maxTurnSampleLimit,
            max(preRollSampleLimit, maxTransitionSamples)
        )
    }

    static func loadAudio(
        fromPath audioFilePath: String,
        channelMode: ChannelMode,
        startTime: Double?,
        endTime: Double?,
        maxReadFrameSize: AVAudioFrameCount?
    ) throws -> AVAudioPCMBuffer {
        try AudioProcessor.loadAudio(
            fromPath: audioFilePath,
            channelMode: channelMode,
            startTime: startTime,
            endTime: endTime,
            maxReadFrameSize: maxReadFrameSize
        )
    }

    static func loadAudio(
        at audioPaths: [String],
        channelMode: ChannelMode
    ) async -> [Result<[Float], Error>] {
        await AudioProcessor.loadAudio(
            at: audioPaths,
            channelMode: channelMode
        )
    }

    static func padOrTrimAudio(
        fromArray audioArray: [Float],
        startAt startIndex: Int,
        toLength frameLength: Int,
        saveSegment: Bool
    ) -> MLMultiArray? {
        AudioProcessor.padOrTrimAudio(
            fromArray: audioArray,
            startAt: startIndex,
            toLength: frameLength,
            saveSegment: saveSegment
        )
    }

    func padOrTrim(
        fromArray audioArray: [Float],
        startAt startIndex: Int,
        toLength frameLength: Int
    ) -> (any AudioProcessorOutputType)? {
        Self.padOrTrimAudio(
            fromArray: audioArray,
            startAt: startIndex,
            toLength: frameLength,
            saveSegment: false
        )
    }

    func append(_ samples: [Float]) {
        guard !samples.isEmpty else {
            return
        }

        let delivery = stateLock.withLock {
            let relativeEnergyWindow = state.relativeEnergyWindow
            state.capturePosition &+= UInt64(samples.count)
            append(samples, to: &state.preRoll, limit: preRollSampleLimit)

            var failure: FailureHandler?
            if state.isTransitioning, !state.hasFailed {
                if canAppend(
                    samples,
                    toCount: state.transitionSamples.count,
                    limit: maxTransitionSampleLimit
                ) {
                    state.transitionSamples.append(contentsOf: samples)
                } else {
                    state.hasFailed = true
                    state.isRecording = false
                    state.isPaused = true
                    state.callback = nil
                    failure = state.failureHandler
                }
            }

            var callback: (([Float]) -> Void)?
            if state.isRecording, !state.isPaused, !state.hasFailed {
                if canAppend(
                    samples,
                    toCount: state.audioSamples.count,
                    limit: maxTurnSampleLimit
                ) {
                    state.audioSamples.append(contentsOf: samples)
                    appendEnergy(
                        for: samples,
                        relativeEnergyWindow: relativeEnergyWindow,
                        to: &state.energy
                    )
                    callback = state.callback
                } else {
                    state.hasFailed = true
                    state.isRecording = false
                    state.isPaused = true
                    state.callback = nil
                    failure = state.failureHandler
                }
            } else if !state.audioSamples.isEmpty {
                state.audioHasGap = true
            }
            return (callback: callback, failure: failure)
        }
        delivery.failure?()
        delivery.callback?(samples)
    }

    func purgeAudioSamples(keepingLast keep: Int) {
        stateLock.withLock {
            let retained = max(0, keep)
            if state.audioSamples.count > retained {
                let removed = state.audioSamples.count - retained
                state.audioSamples.removeFirst(removed)
                state.audioStartPosition &+= UInt64(removed)
            }
        }
    }

    func startRecordingLive(
        inputDeviceID _: DeviceID?,
        callback: (([Float]) -> Void)?
    ) throws {
        let hasFailed = stateLock.withLock {
            guard !state.hasFailed else {
                return true
            }
            if state.isTransitioning {
                state.audioSamples = state.transitionSamples
            } else {
                state.audioSamples = state.preRoll
            }
            // Pre-roll and transition audio both run up to the latest sample.
            state.audioStartPosition = state.capturePosition
                &- UInt64(state.audioSamples.count)
            state.audioHasGap = false
            state.energy = energyEntries(
                for: state.audioSamples,
                relativeEnergyWindow: state.relativeEnergyWindow
            )
            state.transitionSamples = []
            state.isTransitioning = false
            state.callback = callback
            state.isRecording = true
            state.isPaused = false
            return false
        }
        if hasFailed {
            throw TurnAudioProcessorError.capacityExceeded
        }
    }

    func startStreamingRecordingLive(
        inputDeviceID: DeviceID?
    ) -> (
        AsyncThrowingStream<[Float], Error>,
        AsyncThrowingStream<[Float], Error>.Continuation
    ) {
        let (stream, continuation) = AsyncThrowingStream<
            [Float],
            Error
        >.makeStream(bufferingPolicy: .bufferingNewest(8))
        continuation.onTermination = { [weak self] _ in
            self?.stopRecording()
        }
        do {
            try startRecordingLive(inputDeviceID: inputDeviceID) { samples in
                continuation.yield(samples)
            }
        } catch {
            continuation.finish(throwing: error)
        }
        return (stream, continuation)
    }

    func pauseRecording() {
        stateLock.withLock {
            state.isPaused = true
        }
    }

    func resumeRecordingLive(
        inputDeviceID _: DeviceID?,
        callback: (([Float]) -> Void)?
    ) throws {
        let hasFailed = stateLock.withLock {
            guard !state.hasFailed else {
                return true
            }
            if let callback {
                state.callback = callback
            }
            state.isRecording = true
            state.isPaused = false
            return false
        }
        if hasFailed {
            throw TurnAudioProcessorError.capacityExceeded
        }
    }

    func stopRecording() {
        stateLock.withLock {
            state.audioSamples = []
            state.audioStartPosition = state.capturePosition
            state.audioHasGap = false
            state.energy = []
            state.callback = nil
            state.isRecording = false
            state.isPaused = true
        }
    }

    /// The capture position just past the latest appended sample. A voice
    /// window that ends `n` samples before the latest one ends at this
    /// position minus `n`.
    var capturePosition: UInt64 {
        stateLock.withLock {
            state.capturePosition
        }
    }

    /// Records where the turn's speech lies, from the speech gate's
    /// transition at a voice window that ended at `capturePosition`: the
    /// turn's first speech start and its last speech end.
    func observe(
        _ transition: RecognitionSpeechTransition,
        windowEndingAt capturePosition: UInt64
    ) {
        stateLock.withLock {
            switch transition {
            case .started:
                if state.firstSpeechStart == nil {
                    state.firstSpeechStart = capturePosition
                }
            case .ended:
                state.lastSpeechEnd = capturePosition
            case .none, .continued:
                break
            }
        }
    }

    /// The turn that a final-silence reset ends, as a backend hands it on:
    /// only the turn's speech, and the earliest time on the voice window
    /// clock that the next turn's audio can begin. The speech runs from the
    /// turn's first speech start, moved earlier by the speech-start
    /// threshold and the pre-roll so the onset is kept, to its last speech
    /// end; a turn without both has none. The marks are consumed: what the
    /// speech gate reports from here on belongs to the next turn. The next
    /// turn starts with the pre-roll; a voice window's time is when it was
    /// processed, never before its samples arrived, and capture arrives in
    /// buffers of up to 8,192 frames (512 ms at 16 kHz), so one second more
    /// than the pre-roll covers every window that can hold its audio.
    func turnEnd(
        configuration: SidecarConfiguration,
        atMilliseconds now: UInt64,
        discontinuities: UInt64
    ) -> RecognitionTurnEnd {
        let lead = UInt64(preRollSampleLimit / 16) + 1_000
        // The gate starts after whole 100 ms windows of speech.
        let thresholdSamples = (configuration.speechStartMilliseconds + 99)
            / 100 * 1_600
        let speech = stateLock.withLock {
            defer {
                state.firstSpeechStart = nil
                state.lastSpeechEnd = nil
            }
            guard !state.audioHasGap,
                let range = TurnAudioSnapshot.speechRange(
                    bufferStart: state.audioStartPosition,
                    bufferCount: state.audioSamples.count,
                    firstSpeechStart: state.firstSpeechStart,
                    lastSpeechEnd: state.lastSpeechEnd,
                    leadSamples: thresholdSamples + UInt64(preRollSampleLimit)
                )
            else {
                return ContiguousArray<Float>()
            }
            return ContiguousArray(state.audioSamples[range])
        }
        return RecognitionTurnEnd(
            samples: speech,
            nextTurnFromMilliseconds: now > lead ? now - lead : 0,
            discontinuities: discontinuities
        )
    }

    func setFailureHandler(_ handler: FailureHandler?) {
        stateLock.withLock {
            state.failureHandler = handler
        }
    }

    func beginTransition() {
        stateLock.withLock {
            guard !state.hasFailed else {
                return
            }
            state.transitionSamples = state.preRoll
            state.isTransitioning = true
        }
    }

    func beginDiscontinuityTransition() {
        stateLock.withLock {
            guard !state.hasFailed else {
                return
            }
            state.preRoll = []
            state.transitionSamples = []
            state.audioSamples = []
            state.audioStartPosition = state.capturePosition
            state.audioHasGap = false
            state.firstSpeechStart = nil
            state.lastSpeechEnd = nil
            state.energy = []
            state.isTransitioning = true
        }
    }

    func cancelTransition() {
        stateLock.withLock {
            state.transitionSamples = []
            state.isTransitioning = false
        }
    }

    func reset() {
        stateLock.withLock {
            state = State()
        }
    }

    private func append(
        _ samples: [Float],
        to buffer: inout ContiguousArray<Float>,
        limit: Int
    ) {
        guard limit > 0 else {
            buffer = []
            return
        }
        buffer.append(contentsOf: samples)
        if buffer.count > limit {
            buffer.removeFirst(buffer.count - limit)
        }
    }

    private func appendEnergy(
        for samples: [Float],
        relativeEnergyWindow: Int,
        to entries: inout [(relative: Float, average: Float)]
    ) {
        let baseline = entries
            .suffix(relativeEnergyWindow)
            .map(\.average)
            .min()
        let average = AudioProcessor.calculateEnergy(of: samples).avg
        let relative = AudioProcessor.calculateRelativeEnergy(
            of: samples,
            relativeTo: baseline
        )
        entries.append((relative: relative, average: average))
    }

    private func energyEntries(
        for samples: ContiguousArray<Float>,
        relativeEnergyWindow: Int
    ) -> [(relative: Float, average: Float)] {
        var entries: [(relative: Float, average: Float)] = []
        let windowSamples = 1_600
        var startIndex = 0
        while startIndex < samples.count {
            let endIndex = min(startIndex + windowSamples, samples.count)
            appendEnergy(
                for: Array(samples[startIndex..<endIndex]),
                relativeEnergyWindow: relativeEnergyWindow,
                to: &entries
            )
            startIndex = endIndex
        }
        return entries
    }

    private func canAppend(
        _ samples: [Float],
        toCount count: Int,
        limit: Int
    ) -> Bool {
        samples.count <= limit - count
    }
}
