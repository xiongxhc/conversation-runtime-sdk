@preconcurrency import AVFoundation
import CoreML
import Foundation
import VoiceSidecarCore
@preconcurrency import WhisperKit

public final class VoiceProcessingAudioProcessor:
    AudioProcessing,
    @unchecked Sendable
{
    public typealias VoiceWindowHandler = @Sendable ([Float]) -> Void

    private struct State {
        var audioSamples: ContiguousArray<Float> = []
        var energy: [(relative: Float, average: Float)] = []
        var relativeEnergyWindow = 20
        var callback: (([Float]) -> Void)?
        var voiceWindowHandler: VoiceWindowHandler?
        var windowSamples: [Float] = []
        var paused = true
        var recordingStartWaiters: [CheckedContinuation<Void, Never>] = []
    }

    private static let voiceWindowSamples = 1_600

    private let engine: VoiceProcessingEngine
    private let stateLock = NSLock()
    private var state = State()

    public var audioSamples: ContiguousArray<Float> {
        stateLock.withLock {
            state.audioSamples
        }
    }

    public var relativeEnergy: [Float] {
        stateLock.withLock {
            state.energy.map(\.relative)
        }
    }

    public var relativeEnergyWindow: Int {
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

    public init(engine: VoiceProcessingEngine) {
        self.engine = engine
    }

    public static func loadAudio(
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

    public static func loadAudio(
        at audioPaths: [String],
        channelMode: ChannelMode
    ) async -> [Result<[Float], Error>] {
        await AudioProcessor.loadAudio(
            at: audioPaths,
            channelMode: channelMode
        )
    }

    public static func padOrTrimAudio(
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

    public func padOrTrim(
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

    public func setVoiceWindowHandler(_ handler: VoiceWindowHandler?) {
        stateLock.withLock {
            state.voiceWindowHandler = handler
        }
    }

    public func waitUntilRecordingStarted() async {
        let shouldWait = stateLock.withLock {
            state.paused
        }
        guard shouldWait else {
            return
        }
        await withCheckedContinuation { continuation in
            let resumeImmediately = stateLock.withLock {
                guard state.paused else {
                    return true
                }
                state.recordingStartWaiters.append(continuation)
                return false
            }
            if resumeImmediately {
                continuation.resume()
            }
        }
    }

    public func purgeAudioSamples(keepingLast keep: Int) {
        stateLock.withLock {
            let retained = max(0, keep)
            if state.audioSamples.count > retained {
                state.audioSamples.removeFirst(
                    state.audioSamples.count - retained
                )
            }
        }
    }

    public func startRecordingLive(
        inputDeviceID _: DeviceID?,
        callback: (([Float]) -> Void)?
    ) throws {
        let waiters = stateLock.withLock {
            state.audioSamples = []
            state.energy = []
            state.windowSamples = []
            state.callback = callback
            state.paused = false
            let values = state.recordingStartWaiters
            state.recordingStartWaiters.removeAll()
            return values
        }
        engine.setCaptureHandler { [weak self] buffer in
            self?.process(buffer)
        }
        for waiter in waiters {
            waiter.resume()
        }
    }

    public func startStreamingRecordingLive(
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

    public func pauseRecording() {
        stateLock.withLock {
            state.paused = true
        }
    }

    public func resumeRecordingLive(
        inputDeviceID _: DeviceID?,
        callback: (([Float]) -> Void)?
    ) throws {
        stateLock.withLock {
            if let callback {
                state.callback = callback
            }
            state.paused = false
        }
    }

    public func stopRecording() {
        stateLock.withLock {
            state.paused = true
            state.callback = nil
            state.voiceWindowHandler = nil
            state.windowSamples = []
        }
        engine.setCaptureHandler(nil)
    }

    private func process(_ buffer: AVAudioPCMBuffer) {
        guard
            let samples = try? PCMConversion.recognizerSamples(
                from: buffer
            ), !samples.isEmpty
        else {
            return
        }

        let delivery = stateLock.withLock {
            guard !state.paused else {
                return (
                    callback: Optional<(([Float]) -> Void)>.none,
                    handler: Optional<VoiceWindowHandler>.none,
                    windows: [[Float]]()
                )
            }
            state.audioSamples.append(contentsOf: samples)
            let energy = AudioProcessor.calculateEnergy(of: samples)
            let baseline = state.energy
                .suffix(state.relativeEnergyWindow)
                .map(\.average)
                .min()
            let relative = AudioProcessor.calculateRelativeEnergy(
                of: samples,
                relativeTo: baseline
            )
            state.energy.append(
                (relative: relative, average: energy.avg)
            )
            state.windowSamples.append(contentsOf: samples)
            var windows: [[Float]] = []
            while state.windowSamples.count >= Self.voiceWindowSamples {
                windows.append(
                    Array(
                        state.windowSamples.prefix(
                            Self.voiceWindowSamples
                        )
                    )
                )
                state.windowSamples.removeFirst(Self.voiceWindowSamples)
            }
            return (
                callback: state.callback,
                handler: state.voiceWindowHandler,
                windows: windows
            )
        }

        delivery.callback?(samples)
        for window in delivery.windows {
            delivery.handler?(window)
        }
    }
}
