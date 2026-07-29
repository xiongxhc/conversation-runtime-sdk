@preconcurrency import AVFoundation
import Foundation
import VoiceSidecarCore

public enum PCMConversionError: Error, Equatable, Sendable {
    case unsupportedFormat
    case invalidBuffer
}

public enum PCMConversion {
    public static func playbackBuffer(
        from frame: PCMFrame
    ) throws -> AVAudioPCMBuffer {
        let commonFormat: AVAudioCommonFormat
        switch frame.format.sampleFormat {
        case .signed16LittleEndian:
            commonFormat = .pcmFormatInt16
        case .float32LittleEndian:
            commonFormat = .pcmFormatFloat32
        }
        guard
            let format = AVAudioFormat(
                commonFormat: commonFormat,
                sampleRate: Double(frame.format.sampleRateHz),
                channels: AVAudioChannelCount(frame.format.channels),
                interleaved: true
            )
        else {
            throw PCMConversionError.invalidBuffer
        }
        let frameCount = frame.bytes.count / frame.format.frameAlignmentBytes
        guard
            let buffer = AVAudioPCMBuffer(
                pcmFormat: format,
                frameCapacity: AVAudioFrameCount(frameCount)
            )
        else {
            throw PCMConversionError.invalidBuffer
        }
        buffer.frameLength = AVAudioFrameCount(frameCount)
        let destination = buffer.mutableAudioBufferList.pointee.mBuffers
        guard let data = destination.mData,
            Int(destination.mDataByteSize) >= frame.bytes.count
        else {
            throw PCMConversionError.invalidBuffer
        }
        frame.bytes.copyBytes(
            to: data.assumingMemoryBound(to: UInt8.self),
            count: frame.bytes.count
        )
        buffer.mutableAudioBufferList.pointee.mBuffers.mDataByteSize =
            UInt32(frame.bytes.count)
        return buffer
    }

    public static func playbackBuffer(
        from frame: PCMFrame,
        convertedTo outputFormat: AVAudioFormat
    ) throws -> AVAudioPCMBuffer {
        guard outputFormat.commonFormat == .pcmFormatFloat32,
            !outputFormat.isInterleaved,
            outputFormat.sampleRate > 0,
            outputFormat.channelCount > 0
        else {
            throw PCMConversionError.unsupportedFormat
        }
        let channels = try decodedChannels(from: frame)
        let outputFrameCount = max(
            1,
            Int(
                (Double(channels[0].count)
                    * outputFormat.sampleRate
                    / Double(frame.format.sampleRateHz)).rounded(.down)
            )
        )
        guard
            let output = AVAudioPCMBuffer(
                pcmFormat: outputFormat,
                frameCapacity: AVAudioFrameCount(outputFrameCount)
            ), let outputChannels = output.floatChannelData
        else {
            throw PCMConversionError.invalidBuffer
        }
        output.frameLength = AVAudioFrameCount(outputFrameCount)

        for outputChannel in 0..<Int(outputFormat.channelCount) {
            let source = sourceChannel(
                outputChannel,
                outputChannelCount: Int(outputFormat.channelCount),
                channels: channels
            )
            resample(
                source,
                from: Double(frame.format.sampleRateHz),
                to: outputFormat.sampleRate,
                into: outputChannels[outputChannel],
                count: outputFrameCount
            )
        }
        return output
    }

    public static func recognizerSamples(
        from buffer: AVAudioPCMBuffer
    ) throws -> [Float] {
        guard buffer.frameLength > 0,
            buffer.format.sampleRate > 0,
            buffer.format.channelCount > 0
        else {
            throw PCMConversionError.invalidBuffer
        }
        let channels = try decodedChannels(from: buffer)
        let mono = mixToMono(channels)
        guard buffer.format.sampleRate != 16_000 else {
            return mono
        }
        let outputCount = max(
            1,
            Int(
                (Double(mono.count)
                    * 16_000
                    / buffer.format.sampleRate).rounded(.down)
            )
        )
        var output = [Float](repeating: 0, count: outputCount)
        output.withUnsafeMutableBufferPointer { destination in
            resample(
                mono,
                from: buffer.format.sampleRate,
                to: 16_000,
                into: destination.baseAddress!,
                count: outputCount
            )
        }
        return output
    }

    private static func decodedChannels(
        from frame: PCMFrame
    ) throws -> [[Float]] {
        let channelCount = Int(frame.format.channels)
        let frameCount = frame.bytes.count / frame.format.frameAlignmentBytes
        var channels = Array(
            repeating: [Float](repeating: 0, count: frameCount),
            count: channelCount
        )
        frame.bytes.withUnsafeBytes { bytes in
            for frameIndex in 0..<frameCount {
                for channelIndex in 0..<channelCount {
                    let sampleIndex = frameIndex * channelCount + channelIndex
                    switch frame.format.sampleFormat {
                    case .signed16LittleEndian:
                        let bits = Int16(
                            littleEndian: bytes.loadUnaligned(
                                fromByteOffset: sampleIndex * 2,
                                as: Int16.self
                            )
                        )
                        channels[channelIndex][frameIndex] =
                            Float(bits) / 32_768
                    case .float32LittleEndian:
                        let bits = UInt32(
                            littleEndian: bytes.loadUnaligned(
                                fromByteOffset: sampleIndex * 4,
                                as: UInt32.self
                            )
                        )
                        channels[channelIndex][frameIndex] =
                            Float(bitPattern: bits)
                    }
                }
            }
        }
        return channels
    }

    private static func decodedChannels(
        from buffer: AVAudioPCMBuffer
    ) throws -> [[Float]] {
        let frameCount = Int(buffer.frameLength)
        let channelCount = Int(buffer.format.channelCount)
        var channels = Array(
            repeating: [Float](repeating: 0, count: frameCount),
            count: channelCount
        )
        switch buffer.format.commonFormat {
        case .pcmFormatFloat32:
            if buffer.format.isInterleaved {
                let source = try interleavedPointer(
                    buffer,
                    as: Float.self
                )
                for frameIndex in 0..<frameCount {
                    for channelIndex in 0..<channelCount {
                        channels[channelIndex][frameIndex] =
                            source[frameIndex * channelCount + channelIndex]
                    }
                }
            } else {
                guard let source = buffer.floatChannelData else {
                    throw PCMConversionError.invalidBuffer
                }
                for channelIndex in 0..<channelCount {
                    channels[channelIndex] = Array(
                        UnsafeBufferPointer(
                            start: source[channelIndex],
                            count: frameCount
                        )
                    )
                }
            }
        case .pcmFormatInt16:
            if buffer.format.isInterleaved {
                let source = try interleavedPointer(
                    buffer,
                    as: Int16.self
                )
                for frameIndex in 0..<frameCount {
                    for channelIndex in 0..<channelCount {
                        channels[channelIndex][frameIndex] =
                            Float(
                                source[
                                    frameIndex * channelCount + channelIndex
                                ]
                            ) / 32_768
                    }
                }
            } else {
                guard let source = buffer.int16ChannelData else {
                    throw PCMConversionError.invalidBuffer
                }
                for channelIndex in 0..<channelCount {
                    for frameIndex in 0..<frameCount {
                        channels[channelIndex][frameIndex] =
                            Float(source[channelIndex][frameIndex]) / 32_768
                    }
                }
            }
        default:
            throw PCMConversionError.unsupportedFormat
        }
        return channels
    }

    private static func interleavedPointer<Element>(
        _ buffer: AVAudioPCMBuffer,
        as _: Element.Type
    ) throws -> UnsafePointer<Element> {
        let audioBuffer = buffer.audioBufferList.pointee.mBuffers
        guard let data = audioBuffer.mData else {
            throw PCMConversionError.invalidBuffer
        }
        return UnsafePointer(data.assumingMemoryBound(to: Element.self))
    }

    private static func mixToMono(_ channels: [[Float]]) -> [Float] {
        guard channels.count > 1 else {
            return channels[0]
        }
        var mono = [Float](repeating: 0, count: channels[0].count)
        for channel in channels {
            for index in mono.indices {
                mono[index] += channel[index]
            }
        }
        let divisor = Float(channels.count)
        for index in mono.indices {
            mono[index] /= divisor
        }
        return mono
    }

    private static func sourceChannel(
        _ outputChannel: Int,
        outputChannelCount: Int,
        channels: [[Float]]
    ) -> [Float] {
        if channels.count == 1 {
            return channels[0]
        }
        if channels.count == outputChannelCount {
            return channels[outputChannel]
        }
        return mixToMono(channels)
    }

    private static func resample(
        _ source: [Float],
        from sourceRate: Double,
        to destinationRate: Double,
        into destination: UnsafeMutablePointer<Float>,
        count: Int
    ) {
        let step = sourceRate / destinationRate
        for index in 0..<count {
            let position = Double(index) * step
            let lower = min(Int(position), source.count - 1)
            let upper = min(lower + 1, source.count - 1)
            let fraction = Float(position - Double(lower))
            destination[index] =
                source[lower] + (source[upper] - source[lower]) * fraction
        }
    }
}
