import AVFoundation
import Foundation
import Testing

@testable import VoiceSidecarCore
@testable import VoiceSidecarMacOS

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

    #expect(ring.copyFromTap(try int16Buffer(format: format, samples: [1])))
    #expect(ring.copyFromTap(try int16Buffer(format: format, samples: [2])))
    #expect(!ring.copyFromTap(try int16Buffer(format: format, samples: [3])))

    var received: [Int16] = []
    ring.drain { buffer in
        received.append(interleavedSamples(from: buffer, as: Int16.self)[0])
    }

    #expect(received == [1, 2])
    #expect(ring.droppedBufferCount == 1)
    #expect(ring.copyFromTap(try int16Buffer(format: format, samples: [4])))
}

@Test(
    arguments: [
        StreamingRecognizerCase(sampleFormat: .signed16LittleEndian, channels: 1),
        StreamingRecognizerCase(sampleFormat: .signed16LittleEndian, channels: 2),
        StreamingRecognizerCase(sampleFormat: .float32LittleEndian, channels: 1),
        StreamingRecognizerCase(sampleFormat: .float32LittleEndian, channels: 2),
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
                    frameCount: 4_096
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

private func streamingBuffer(
    sampleFormat: PCMSampleFormat,
    channels: AVAudioChannelCount,
    startFrame: Int,
    frameCount: Int,
    sampleRate: Double = 48_000
) -> AVAudioPCMBuffer {
    let commonFormat: AVAudioCommonFormat =
        sampleFormat == .signed16LittleEndian
        ? .pcmFormatInt16
        : .pcmFormatFloat32
    let format = AVAudioFormat(
        commonFormat: commonFormat,
        sampleRate: sampleRate,
        channels: channels,
        interleaved: false
    )!
    let buffer = AVAudioPCMBuffer(
        pcmFormat: format,
        frameCapacity: AVAudioFrameCount(frameCount)
    )!
    buffer.frameLength = AVAudioFrameCount(frameCount)
    for channel in 0..<Int(channels) {
        for frame in 0..<frameCount {
            let sourceFrame = startFrame + frame
            switch sampleFormat {
            case .signed16LittleEndian:
                let base = Int16(sourceFrame % 8_192 - 4_096)
                let offset: Int16 = channels == 1 ? 0 : (channel == 0 ? -64 : 64)
                buffer.int16ChannelData![channel][frame] = base + offset
            case .float32LittleEndian:
                let base = Float(sourceFrame) / 20_000 - 0.5
                let offset: Float = channels == 1 ? 0 : (channel == 0 ? -0.1 : 0.1)
                buffer.floatChannelData![channel][frame] = base + offset
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
