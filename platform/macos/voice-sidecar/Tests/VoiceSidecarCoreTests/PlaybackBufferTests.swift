import Foundation
import Testing
@testable import VoiceSidecarCore

@Test
func sequencesRemainOrderedWithinOneUtterance() throws {
    var buffer = PlaybackBuffer()
    try buffer.enqueue(pcmFrame(sequence: 0))

    #expect(throws: PlaybackBufferError.sequenceGap(expected: 1, received: 2)) {
        try buffer.enqueue(pcmFrame(sequence: 2))
    }

    try buffer.enqueue(pcmFrame(sequence: 1))
    #expect(buffer.frameCount == 2)
}

@Test
func aNewUtteranceStartsAtZeroAndOldStreamsCannotResume() throws {
    var buffer = PlaybackBuffer()
    try buffer.enqueue(pcmFrame(utteranceID: 1, sequence: 0))
    try buffer.enqueue(pcmFrame(utteranceID: 2, sequence: 0))

    #expect(throws: PlaybackBufferError.staleUtterance) {
        try buffer.enqueue(pcmFrame(utteranceID: 1, sequence: 1))
    }
    #expect(throws: PlaybackBufferError.sequenceGap(expected: 0, received: 1)) {
        try buffer.enqueue(pcmFrame(utteranceID: 3, sequence: 1))
    }
}

@Test
func turnAndUtteranceIdentityRemainMonotonicWithinGeneration() throws {
    var buffer = PlaybackBuffer()
    try buffer.enqueue(pcmFrame(turnID: 7, generationID: 3, utteranceID: 2))

    #expect(throws: PlaybackBufferError.turnIdentityChanged) {
        try buffer.enqueue(
            pcmFrame(turnID: 8, generationID: 3, utteranceID: 3)
        )
    }
    #expect(throws: PlaybackBufferError.staleUtterance) {
        try buffer.enqueue(
            pcmFrame(turnID: 7, generationID: 3, utteranceID: 1)
        )
    }
}

@Test
func negotiatedFormatCannotChangeEvenAfterFlush() throws {
    var buffer = PlaybackBuffer()
    try buffer.enqueue(pcmFrame())
    _ = try buffer.flush(throughGenerationID: 1)
    let changed = PCMFormat(
        sampleRateHz: 48_000,
        channels: 1,
        sampleFormat: .signed16LittleEndian
    )

    #expect(throws: PlaybackBufferError.formatChanged) {
        try buffer.enqueue(pcmFrame(generationID: 2, format: changed))
    }
}

@Test
func flushedAndOlderGenerationsAreStale() throws {
    var buffer = PlaybackBuffer()
    try buffer.enqueue(pcmFrame(generationID: 2))
    _ = try buffer.flush(throughGenerationID: 2)

    #expect(throws: PlaybackBufferError.staleGeneration) {
        try buffer.enqueue(pcmFrame(generationID: 1))
    }
    #expect(throws: PlaybackBufferError.staleGeneration) {
        try buffer.enqueue(pcmFrame(generationID: 2))
    }

    try buffer.enqueue(pcmFrame(generationID: 3))
    #expect(buffer.activeGenerationID == 3)
}

@Test
func oneHundredFrameLimitAppliesIndependently() throws {
    let fastFormat = PCMFormat(
        sampleRateHz: 10_000_000,
        channels: 1,
        sampleFormat: .signed16LittleEndian
    )
    var buffer = PlaybackBuffer()

    for sequence in 0..<100 {
        try buffer.enqueue(
            pcmFrame(
                sequence: UInt64(sequence),
                format: fastFormat,
                byteCount: 2
            )
        )
    }

    #expect(throws: PlaybackBufferError.frameLimitExceeded(maximum: 100)) {
        try buffer.enqueue(pcmFrame(sequence: 100, format: fastFormat, byteCount: 2))
    }
}

@Test
func twoSecondDurationLimitAppliesIndependently() throws {
    let format = PCMFormat(
        sampleRateHz: 12_000,
        channels: 1,
        sampleFormat: .signed16LittleEndian
    )
    var buffer = PlaybackBuffer()

    try buffer.enqueue(pcmFrame(format: format, byteCount: 48_000))
    #expect(buffer.queuedDurationNanoseconds == 2_000_000_000)
    #expect(
        throws: PlaybackBufferError.durationLimitExceeded(
            maximumNanoseconds: 2_000_000_000
        )
    ) {
        try buffer.enqueue(pcmFrame(sequence: 1, format: format, byteCount: 2))
    }
}

@Test
func renderedFramesMustLeaveInQueueOrder() throws {
    var buffer = PlaybackBuffer()
    let first = try pcmFrame(sequence: 0)
    let second = try pcmFrame(sequence: 1)
    try buffer.enqueue(first)
    try buffer.enqueue(second)

    #expect(throws: PlaybackBufferError.renderOrderMismatch) {
        try buffer.markRendered(second.identity)
    }

    try buffer.markRendered(first.identity)
    #expect(buffer.frameCount == 1)
    try buffer.markRendered(second.identity)
    #expect(buffer.isPlaybackActive == false)
}

@Test
func flushMayAdvanceBeforeTheFirstMediaFrame() throws {
    var buffer = PlaybackBuffer()

    #expect(try buffer.flush(throughGenerationID: 1).isEmpty)
    try buffer.enqueue(pcmFrame(generationID: 2))
    #expect(buffer.activeGenerationID == 2)
}
