import AVFoundation
import Darwin
import Foundation
import Testing

@testable import VoiceSidecarCore
@testable import VoiceSidecarMacOS

@Test
func turnAudioProcessorCapsInactivePreRollAndStartsFromItsTail() throws {
    let processor = TurnAudioProcessor(preRollSamples: 4)
    processor.append([1, 2, 3])
    processor.append([4, 5, 6])

    try processor.startRecordingLive(inputDeviceID: nil, callback: nil)

    #expect(Array(processor.audioSamples) == [3, 4, 5, 6])
}

@Test
func turnAudioProcessorRestartDropsClosedTurnButKeepsRestartSpeech() throws {
    let processor = TurnAudioProcessor(preRollSamples: 4)
    try processor.startRecordingLive(inputDeviceID: nil, callback: nil)
    processor.append([1, 2, 3, 4])
    processor.stopRecording()
    processor.append([5, 6])

    try processor.startRecordingLive(inputDeviceID: nil, callback: nil)

    #expect(Array(processor.audioSamples) == [3, 4, 5, 6])
}

@Test
func turnAudioProcessorBoundsPreRollButRetainsActiveEnergyHistory() throws {
    let processor = TurnAudioProcessor(
        preRollSamples: 4
    )
    for value in 0..<10 {
        processor.append([Float(value)])
    }

    try processor.startRecordingLive(inputDeviceID: nil, callback: nil)

    #expect(Array(processor.audioSamples) == [6, 7, 8, 9])
    #expect(processor.relativeEnergy.count == 1)

    for value in 10..<20 {
        processor.append([Float(value)])
    }

    #expect(processor.relativeEnergy.count == 11)
}

@Test
func turnAudioProcessorTransitionRetainsMoreThanPreRoll() throws {
    let processor = TurnAudioProcessor(
        preRollSamples: 2,
        maxTransitionSamples: 6
    )
    processor.append([1, 2])
    try processor.startRecordingLive(inputDeviceID: nil, callback: nil)
    processor.beginTransition()
    processor.stopRecording()
    processor.append([3, 4])
    processor.append([5, 6])

    try processor.startRecordingLive(inputDeviceID: nil, callback: nil)

    #expect(Array(processor.audioSamples) == [1, 2, 3, 4, 5, 6])
}

@Test
func turnAudioProcessorFailsClosedAtTheTurnLimit() throws {
    let processor = TurnAudioProcessor(maxTurnSamples: 4)
    let recorder = VoiceWindowRecorder()
    processor.setFailureHandler {
        recorder.record([])
    }
    try processor.startRecordingLive(inputDeviceID: nil, callback: nil)
    processor.append([1, 2, 3, 4])

    processor.append([5])

    #expect(recorder.count == 1)
    #expect(processor.audioSamples.count == 4)
}

@Test
func signed16MonoPlaybackConversionPreservesSamples() throws {
    let samples: [Int16] = [.min, -1, 0, 1, .max]
    let frame = try PCMFrame(
        turnID: 1,
        generationID: 2,
        utteranceID: 3,
        sequence: 0,
        format: PCMFormat(
            sampleRateHz: 16_000,
            channels: 1,
            sampleFormat: .signed16LittleEndian
        ),
        bytes: data(of: samples)
    )

    let buffer = try PCMConversion.playbackBuffer(from: frame)

    #expect(buffer.format.commonFormat == .pcmFormatInt16)
    #expect(buffer.format.sampleRate == 16_000)
    #expect(buffer.format.channelCount == 1)
    #expect(buffer.format.isInterleaved)
    #expect(buffer.frameLength == AVAudioFrameCount(samples.count))
    #expect(interleavedSamples(from: buffer, as: Int16.self) == samples)
}

@Test
func float32StereoPlaybackConversionPreservesInterleaving() throws {
    let samples: [Float] = [0.25, -0.25, 0.5, -0.5]
    let frame = try PCMFrame(
        turnID: 1,
        generationID: 2,
        utteranceID: 3,
        sequence: 0,
        format: PCMFormat(
            sampleRateHz: 24_000,
            channels: 2,
            sampleFormat: .float32LittleEndian
        ),
        bytes: data(of: samples)
    )

    let buffer = try PCMConversion.playbackBuffer(from: frame)

    #expect(buffer.format.commonFormat == .pcmFormatFloat32)
    #expect(buffer.format.sampleRate == 24_000)
    #expect(buffer.format.channelCount == 2)
    #expect(buffer.format.isInterleaved)
    #expect(buffer.frameLength == 2)
    #expect(interleavedSamples(from: buffer, as: Float.self) == samples)
}

@Test
func recognizerConversionDownmixesStereoAtSixteenKilohertz() throws {
    let format = try #require(
        AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: 16_000,
            channels: 2,
            interleaved: false
        )
    )
    let buffer = try #require(
        AVAudioPCMBuffer(pcmFormat: format, frameCapacity: 3)
    )
    buffer.frameLength = 3
    let channels = try #require(buffer.floatChannelData)
    channels[0][0] = 1
    channels[0][1] = 0.5
    channels[0][2] = -1
    channels[1][0] = -1
    channels[1][1] = 0.5
    channels[1][2] = 1

    let samples = try PCMConversion.recognizerSamples(from: buffer)

    #expect(samples == [0, 0.5, 0])
}

@Test
func recognizerConversionResamplesToSixteenKilohertz() throws {
    let format = try #require(
        AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: 32_000,
            channels: 1,
            interleaved: false
        )
    )
    let buffer = try #require(
        AVAudioPCMBuffer(pcmFormat: format, frameCapacity: 4)
    )
    buffer.frameLength = 4
    let channel = try #require(buffer.floatChannelData?[0])
    channel[0] = 0
    channel[1] = 0.25
    channel[2] = 0.5
    channel[3] = 0.75

    let samples = try PCMConversion.recognizerSamples(from: buffer)

    #expect(samples.count == 2)
    #expect(abs(samples[0] - 0) < 0.000_001)
    #expect(abs(samples[1] - 0.5) < 0.000_001)
}

@Test
func recognizerConversionRejectsUnsupportedSampleFormat() throws {
    let format = try #require(
        AVAudioFormat(
            commonFormat: .pcmFormatInt32,
            sampleRate: 16_000,
            channels: 1,
            interleaved: false
        )
    )
    let buffer = try #require(
        AVAudioPCMBuffer(pcmFormat: format, frameCapacity: 1)
    )
    buffer.frameLength = 1

    #expect(throws: PCMConversionError.unsupportedFormat) {
        _ = try PCMConversion.recognizerSamples(from: buffer)
    }
}

@Test
func captureRingDropsOverflowAndPreservesOrder() throws {
    let format = try #require(
        AVAudioFormat(
            commonFormat: .pcmFormatInt16,
            sampleRate: 48_000,
            channels: 1,
            interleaved: true
        )
    )
    let ring = try CapturePCMBufferRing(
        format: format,
        frameCapacity: 1,
        capacity: 2
    )

    #expect(
        ring.copyFromTap(try int16Buffer(format: format, samples: [1]))
            == .enqueued
    )
    #expect(
        ring.copyFromTap(try int16Buffer(format: format, samples: [2]))
            == .enqueued
    )
    #expect(
        ring.copyFromTap(try int16Buffer(format: format, samples: [3]))
            == .capacityOverflow
    )

    var received: [Int16] = []
    ring.drain { event in
        if case .buffer(let buffer) = event {
            received.append(
                interleavedSamples(from: buffer, as: Int16.self)[0]
            )
        }
    }

    #expect(received == [1, 2])
    #expect(ring.droppedBufferCount == 1)
    #expect(
        ring.copyFromTap(try int16Buffer(format: format, samples: [4]))
            == .enqueued
    )
    var resumedEvents: [CaptureEventSnapshot] = []
    ring.drain { event in
        switch event {
        case .discontinuity:
            resumedEvents.append(.discontinuity)
        case .buffer(let buffer):
            resumedEvents.append(
                .sample(
                    interleavedSamples(
                        from: buffer,
                        as: Int16.self
                    )[0]
                )
            )
        }
    }
    #expect(resumedEvents == [.discontinuity, .sample(4)])
}

@Test
func capturePumpReportsStructuralFormatFaultOffTap() throws {
    let format = try #require(
        AVAudioFormat(
            commonFormat: .pcmFormatInt16,
            sampleRate: 48_000,
            channels: 1,
            interleaved: true
        )
    )
    let changedFormat = try #require(
        AVAudioFormat(
            commonFormat: .pcmFormatInt16,
            sampleRate: 44_100,
            channels: 1,
            interleaved: true
        )
    )
    let ring = try CapturePCMBufferRing(
        format: format,
        frameCapacity: 8,
        capacity: 2
    )
    let recorder = CaptureFaultRecorder()
    let pump = CaptureBufferPump(
        ring: ring,
        eventHandler: { _ in },
        faultHandler: { fault in
            recorder.record(fault)
        }
    )
    defer {
        pump.stop()
    }

    #expect(
        pump.enqueue(
            try int16Buffer(format: changedFormat, samples: [1])
        ) == .formatMismatch
    )
    #expect(recorder.wait() == .formatMismatch)
}

@Test
func capturePumpReportsOversizedStructuralFaultOffTap() throws {
    let format = try #require(
        AVAudioFormat(
            commonFormat: .pcmFormatInt16,
            sampleRate: 48_000,
            channels: 1,
            interleaved: true
        )
    )
    let ring = try CapturePCMBufferRing(
        format: format,
        frameCapacity: 1,
        capacity: 2
    )
    let recorder = CaptureFaultRecorder()
    let pump = CaptureBufferPump(
        ring: ring,
        eventHandler: { _ in },
        faultHandler: { fault in
            recorder.record(fault)
        }
    )
    defer {
        pump.stop()
    }

    #expect(
        pump.enqueue(
            try int16Buffer(format: format, samples: [1, 2])
        ) == .oversizedInput
    )
    #expect(recorder.wait() == .oversizedInput)
}

@Test
func captureRingConcurrentSPSCStressPreservesOrderAndBounds() throws {
    let format = try #require(
        AVAudioFormat(
            commonFormat: .pcmFormatInt16,
            sampleRate: 48_000,
            channels: 1,
            interleaved: true
        )
    )
    let ring = try CapturePCMBufferRing(
        format: format,
        frameCapacity: 1,
        capacity: 16
    )
    let source = try int16Buffer(format: format, samples: [0])
    let sourceSamples = try #require(
        source.mutableAudioBufferList.pointee.mBuffers.mData?
            .assumingMemoryBound(to: Int16.self)
    )
    let iterations = 5_000
    let group = DispatchGroup()
    let producer = DispatchQueue(label: "capture-ring-producer")
    let consumer = DispatchQueue(label: "capture-ring-consumer")
    let received = SPSCResultRecorder()
    let producerState = AtomicProducerState()

    group.enter()
    consumer.async {
        while !producerState.isFinished || ring.queuedBufferCount > 0 {
            ring.drain { event in
                switch event {
                case .discontinuity:
                    received.recordDiscontinuity()
                case .buffer(let buffer):
                    received.record(
                        interleavedSamples(
                            from: buffer,
                            as: Int16.self
                        )[0]
                    )
                }
            }
            sched_yield()
        }
        group.leave()
    }

    group.enter()
    producer.async {
        for value in 0..<iterations {
            sourceSamples[0] = Int16(value)
            while ring.copyFromTap(source) == .capacityOverflow {
                sched_yield()
            }
        }
        producerState.finish()
        group.leave()
    }

    #expect(group.wait(timeout: .now() + 5) == .success)
    #expect(received.samples == (0..<iterations).map(Int16.init))
    #expect(ring.maximumQueuedBufferCount <= 16)
    #expect(ring.queuedBufferCount == 0)
}

@Test
func captureDiscontinuityPreventsVoiceWindowBridging() throws {
    let engine = VoiceProcessingEngine(
        permissionProvider: AuthorizedPermissionProvider()
    )
    let processor = VoiceProcessingAudioProcessor(engine: engine)
    let recorder = VoiceWindowRecorder()
    processor.setVoiceWindowHandler { window in
        recorder.record(window)
    }
    try processor.startRecordingLive(inputDeviceID: nil, callback: nil)
    defer {
        processor.stopRecording()
    }
    let format = try #require(
        AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: 16_000,
            channels: 1,
            interleaved: false
        )
    )

    processor.consumeCaptureEvent(
        .buffer(try floatBuffer(format: format, count: 800))
    )
    processor.consumeCaptureEvent(.discontinuity)
    processor.consumeCaptureEvent(
        .buffer(try floatBuffer(format: format, count: 800))
    )
    #expect(recorder.count == 0)

    processor.consumeCaptureEvent(
        .buffer(try floatBuffer(format: format, count: 800))
    )
    #expect(recorder.count == 1)
}

@Test
func continuousCaptureSurvivesLogicalTurnRotation() throws {
    let engine = VoiceProcessingEngine(
        permissionProvider: AuthorizedPermissionProvider()
    )
    let source = VoiceProcessingAudioProcessor(engine: engine)
    let turn = TurnAudioProcessor()
    let recorder = VoiceWindowRecorder()
    source.setVoiceWindowHandler { window in
        recorder.record(window)
    }
    let format = try #require(
        AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: 16_000,
            channels: 1,
            interleaved: false
        )
    )

    try source.startRecordingLive(inputDeviceID: nil) { samples in
        turn.append(samples)
    }
    defer {
        source.stopRecording()
    }
    try turn.startRecordingLive(inputDeviceID: nil, callback: nil)
    source.consumeCaptureEvent(
        .buffer(try floatBuffer(format: format, count: 1_600))
    )
    turn.stopRecording()
    source.consumeCaptureEvent(
        .buffer(try floatBuffer(format: format, count: 1_600))
    )
    try turn.startRecordingLive(inputDeviceID: nil, callback: nil)
    source.consumeCaptureEvent(
        .buffer(try floatBuffer(format: format, count: 1_600))
    )

    #expect(recorder.count == 3)
    #expect(turn.audioSamples.count == 4_800)
}

@Test
func voiceProcessingAudioProcessorBoundsSessionHistory() throws {
    let engine = VoiceProcessingEngine(
        permissionProvider: AuthorizedPermissionProvider()
    )
    let processor = VoiceProcessingAudioProcessor(engine: engine)
    let format = try #require(
        AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: 16_000,
            channels: 1,
            interleaved: false
        )
    )

    try processor.startRecordingLive(inputDeviceID: nil, callback: nil)
    defer {
        processor.stopRecording()
    }
    for _ in 0..<4 {
        processor.consumeCaptureEvent(
            .buffer(try floatBuffer(format: format, count: 1_600))
        )
    }

    #expect(processor.audioSamples.count == 4_800)
}

@Test(
    arguments: [
        StreamingRecognizerCase(
            sampleFormat: .signed16LittleEndian,
            channels: 1,
            interleaved: false
        ),
        StreamingRecognizerCase(
            sampleFormat: .signed16LittleEndian,
            channels: 2,
            interleaved: false
        ),
        StreamingRecognizerCase(
            sampleFormat: .float32LittleEndian,
            channels: 1,
            interleaved: false
        ),
        StreamingRecognizerCase(
            sampleFormat: .float32LittleEndian,
            channels: 2,
            interleaved: false
        ),
        StreamingRecognizerCase(
            sampleFormat: .signed16LittleEndian,
            channels: 1,
            interleaved: true
        ),
        StreamingRecognizerCase(
            sampleFormat: .signed16LittleEndian,
            channels: 2,
            interleaved: true
        ),
        StreamingRecognizerCase(
            sampleFormat: .float32LittleEndian,
            channels: 1,
            interleaved: true
        ),
        StreamingRecognizerCase(
            sampleFormat: .float32LittleEndian,
            channels: 2,
            interleaved: true
        ),
    ]
)
func streamingRecognizerResamplingPreservesPhase(
    input: StreamingRecognizerCase
) throws {
    let converter = StreamingPCMRecognizerConverter()
    var output: [Float] = []
    for chunk in 0..<3 {
        output.append(
            contentsOf: try converter.convert(
                streamingBuffer(
                    sampleFormat: input.sampleFormat,
                    channels: input.channels,
                    startFrame: chunk * 4_096,
                    frameCount: 4_096,
                    interleaved: input.interleaved
                )
            )
        )
    }

    #expect(output.count == 4_096)
    for index in output.indices {
        let expected = streamingExpectedSample(
            sampleFormat: input.sampleFormat,
            sourceFrame: index * 3
        )
        #expect(abs(output[index] - expected) < 0.000_01)
    }
}

@Test
func streamingRecognizerFormatChangesRequireExplicitReset() throws {
    let converter = StreamingPCMRecognizerConverter()
    _ = try converter.convert(
        streamingBuffer(
            sampleFormat: .float32LittleEndian,
            channels: 1,
            startFrame: 0,
            frameCount: 32,
            sampleRate: 48_000
        )
    )
    let changed = streamingBuffer(
        sampleFormat: .float32LittleEndian,
        channels: 1,
        startFrame: 0,
        frameCount: 32,
        sampleRate: 44_100
    )

    #expect(throws: PCMConversionError.formatChanged) {
        _ = try converter.convert(changed)
    }

    converter.reset()
    #expect(try !converter.convert(changed).isEmpty)
}

struct StreamingRecognizerCase: Sendable {
    let sampleFormat: PCMSampleFormat
    let channels: AVAudioChannelCount
    let interleaved: Bool
}

private enum CaptureEventSnapshot: Equatable {
    case discontinuity
    case sample(Int16)
}

private func int16Buffer(
    format: AVAudioFormat,
    samples: [Int16]
) throws -> AVAudioPCMBuffer {
    let buffer = try #require(
        AVAudioPCMBuffer(
            pcmFormat: format,
            frameCapacity: AVAudioFrameCount(samples.count)
        )
    )
    buffer.frameLength = AVAudioFrameCount(samples.count)
    let destination = try #require(
        buffer.mutableAudioBufferList.pointee.mBuffers.mData?
            .assumingMemoryBound(to: Int16.self)
    )
    for index in samples.indices {
        destination[index] = samples[index]
    }
    buffer.mutableAudioBufferList.pointee.mBuffers.mDataByteSize =
        UInt32(samples.count * MemoryLayout<Int16>.stride)
    return buffer
}

private func floatBuffer(
    format: AVAudioFormat,
    count: Int
) throws -> AVAudioPCMBuffer {
    let buffer = try #require(
        AVAudioPCMBuffer(
            pcmFormat: format,
            frameCapacity: AVAudioFrameCount(count)
        )
    )
    buffer.frameLength = AVAudioFrameCount(count)
    let samples = try #require(buffer.floatChannelData?[0])
    for index in 0..<count {
        samples[index] = 0.5
    }
    return buffer
}

private func streamingBuffer(
    sampleFormat: PCMSampleFormat,
    channels: AVAudioChannelCount,
    startFrame: Int,
    frameCount: Int,
    sampleRate: Double = 48_000,
    interleaved: Bool = false
) -> AVAudioPCMBuffer {
    let commonFormat: AVAudioCommonFormat =
        sampleFormat == .signed16LittleEndian
        ? .pcmFormatInt16
        : .pcmFormatFloat32
    let format = AVAudioFormat(
        commonFormat: commonFormat,
        sampleRate: sampleRate,
        channels: channels,
        interleaved: interleaved
    )!
    let buffer = AVAudioPCMBuffer(
        pcmFormat: format,
        frameCapacity: AVAudioFrameCount(frameCount)
    )!
    buffer.frameLength = AVAudioFrameCount(frameCount)
    for frame in 0..<frameCount {
        for channel in 0..<Int(channels) {
            let sourceFrame = startFrame + frame
            switch sampleFormat {
            case .signed16LittleEndian:
                let base = Int16(sourceFrame % 8_192 - 4_096)
                let offset: Int16 = channels == 1 ? 0 : (channel == 0 ? -64 : 64)
                if interleaved {
                    let samples = buffer.mutableAudioBufferList.pointee
                        .mBuffers.mData!
                        .assumingMemoryBound(to: Int16.self)
                    samples[frame * Int(channels) + channel] = base + offset
                } else {
                    buffer.int16ChannelData![channel][frame] = base + offset
                }
            case .float32LittleEndian:
                let base = Float(sourceFrame) / 20_000 - 0.5
                let offset: Float = channels == 1 ? 0 : (channel == 0 ? -0.1 : 0.1)
                if interleaved {
                    let samples = buffer.mutableAudioBufferList.pointee
                        .mBuffers.mData!
                        .assumingMemoryBound(to: Float.self)
                    samples[frame * Int(channels) + channel] = base + offset
                } else {
                    buffer.floatChannelData![channel][frame] = base + offset
                }
            }
        }
    }
    return buffer
}

private func streamingExpectedSample(
    sampleFormat: PCMSampleFormat,
    sourceFrame: Int
) -> Float {
    switch sampleFormat {
    case .signed16LittleEndian:
        Float(Int16(sourceFrame % 8_192 - 4_096)) / 32_768
    case .float32LittleEndian:
        Float(sourceFrame) / 20_000 - 0.5
    }
}

private func data<Element>(of values: [Element]) -> Data {
    values.withUnsafeBytes { Data($0) }
}

private func interleavedSamples<Element>(
    from buffer: AVAudioPCMBuffer,
    as _: Element.Type
) -> [Element] {
    let audioBuffer = buffer.audioBufferList.pointee.mBuffers
    let count = Int(audioBuffer.mDataByteSize) / MemoryLayout<Element>.stride
    let pointer = audioBuffer.mData!.assumingMemoryBound(to: Element.self)
    return Array(UnsafeBufferPointer(start: pointer, count: count))
}

private final class CaptureFaultRecorder: @unchecked Sendable {
    private let lock = NSLock()
    private let semaphore = DispatchSemaphore(value: 0)
    private var fault: CaptureStructuralFault?

    func record(_ fault: CaptureStructuralFault) {
        lock.withLock {
            self.fault = fault
        }
        semaphore.signal()
    }

    func wait() -> CaptureStructuralFault? {
        guard semaphore.wait(timeout: .now() + 1) == .success else {
            return nil
        }
        return lock.withLock { fault }
    }
}

private final class SPSCResultRecorder: @unchecked Sendable {
    private let lock = NSLock()
    private var values: [Int16] = []
    private var discontinuities = 0

    var samples: [Int16] {
        lock.withLock { values }
    }

    func record(_ sample: Int16) {
        lock.withLock {
            values.append(sample)
        }
    }

    func recordDiscontinuity() {
        lock.withLock {
            discontinuities += 1
        }
    }
}

private final class AtomicProducerState: @unchecked Sendable {
    private let lock = NSLock()
    private var finished = false

    var isFinished: Bool {
        lock.withLock { finished }
    }

    func finish() {
        lock.withLock {
            finished = true
        }
    }
}

private struct AuthorizedPermissionProvider: MicrophonePermissionProviding {
    func authorizationStatus() -> MicrophoneAuthorization {
        .authorized
    }

    func requestAccess() async -> Bool {
        true
    }
}

private final class VoiceWindowRecorder: @unchecked Sendable {
    private let lock = NSLock()
    private var windows: [[Float]] = []

    var count: Int {
        lock.withLock { windows.count }
    }

    func record(_ window: [Float]) {
        lock.withLock {
            windows.append(window)
        }
    }
}
