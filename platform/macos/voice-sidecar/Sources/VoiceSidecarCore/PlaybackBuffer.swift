public enum PlaybackBufferError: Error, Equatable, Sendable {
    case staleGeneration
    case overlappingGeneration
    case turnIdentityChanged
    case staleUtterance
    case sequenceGap(expected: UInt64, received: UInt64)
    case sequenceOverflow
    case formatChanged
    case frameLimitExceeded(maximum: Int)
    case durationLimitExceeded(maximumNanoseconds: UInt64)
    case durationOverflow
    case renderOrderMismatch
}

public struct PlaybackBuffer: Sendable {
    public static let maximumFrames = 100
    public static let maximumDurationNanoseconds: UInt64 = 2_000_000_000

    private struct StreamIdentity: Equatable, Sendable {
        let turnID: UInt64
        let generationID: UInt64
        let utteranceID: UInt64

        init(_ frame: PCMFrame) {
            turnID = frame.turnID
            generationID = frame.generationID
            utteranceID = frame.utteranceID
        }
    }

    private struct QueuedFrame: Sendable {
        let frame: PCMFrame
        let durationNanoseconds: UInt64
    }

    private var frames: [QueuedFrame] = []
    private var negotiatedFormat: PCMFormat?
    private var latestGenerationID: UInt64?
    private var flushedThroughGenerationID: UInt64?
    private var currentStream: StreamIdentity?
    private var closedThroughUtteranceID: UInt64?
    private var nextSequence: UInt64 = 0

    public init() {}

    public var frameCount: Int {
        frames.count
    }

    public private(set) var queuedDurationNanoseconds: UInt64 = 0

    public var isPlaybackActive: Bool {
        !frames.isEmpty
    }

    public var activeGenerationID: UInt64? {
        frames.first?.frame.generationID
    }

    var retainedStreamHistoryCount: Int {
        closedThroughUtteranceID == nil ? 0 : 1
    }

    func contains(_ identity: PlaybackFrameIdentity) -> Bool {
        frames.contains { $0.frame.identity == identity }
    }

    func isExplicitlyStale(_ identity: PlaybackFrameIdentity) -> Bool {
        guard let flushedThroughGenerationID else {
            return false
        }
        return identity.generationID <= flushedThroughGenerationID
    }

    public mutating func enqueue(_ frame: PCMFrame) throws {
        try validateGeneration(frame.generationID)
        if let negotiatedFormat, frame.format != negotiatedFormat {
            throw PlaybackBufferError.formatChanged
        }
        guard frames.count < Self.maximumFrames else {
            throw PlaybackBufferError.frameLimitExceeded(maximum: Self.maximumFrames)
        }

        let duration = try frameDurationNanoseconds(frame)
        let (nextDuration, overflow) = queuedDurationNanoseconds.addingReportingOverflow(duration)
        guard !overflow else {
            throw PlaybackBufferError.durationOverflow
        }
        guard nextDuration <= Self.maximumDurationNanoseconds else {
            throw PlaybackBufferError.durationLimitExceeded(
                maximumNanoseconds: Self.maximumDurationNanoseconds
            )
        }

        let advancesGeneration = latestGenerationID.map {
            frame.generationID > $0
        } ?? false
        let stream = StreamIdentity(frame)
        var nextCurrentStream = advancesGeneration ? nil : currentStream
        var nextClosedThroughUtteranceID = advancesGeneration
            ? nil
            : closedThroughUtteranceID
        var expectedSequence = advancesGeneration ? 0 : nextSequence
        if nextCurrentStream != stream {
            if let nextClosedThroughUtteranceID,
               frame.utteranceID <= nextClosedThroughUtteranceID
            {
                throw PlaybackBufferError.staleUtterance
            }
            if let nextCurrentStream,
               nextCurrentStream.generationID == frame.generationID
            {
                guard nextCurrentStream.turnID == frame.turnID else {
                    throw PlaybackBufferError.turnIdentityChanged
                }
                guard frame.utteranceID > nextCurrentStream.utteranceID else {
                    throw PlaybackBufferError.staleUtterance
                }
            }
            guard frame.sequence == 0 else {
                throw PlaybackBufferError.sequenceGap(expected: 0, received: frame.sequence)
            }
            if let nextCurrentStream {
                nextClosedThroughUtteranceID = nextCurrentStream.utteranceID
            }
            nextCurrentStream = stream
            expectedSequence = 0
        }
        guard frame.sequence == expectedSequence else {
            throw PlaybackBufferError.sequenceGap(
                expected: expectedSequence,
                received: frame.sequence
            )
        }
        guard frame.sequence < UInt64.max else {
            throw PlaybackBufferError.sequenceOverflow
        }

        negotiatedFormat = negotiatedFormat ?? frame.format
        if latestGenerationID == nil || frame.generationID > latestGenerationID! {
            latestGenerationID = frame.generationID
        }
        closedThroughUtteranceID = nextClosedThroughUtteranceID
        currentStream = nextCurrentStream
        nextSequence = frame.sequence + 1
        queuedDurationNanoseconds = nextDuration
        frames.append(QueuedFrame(frame: frame, durationNanoseconds: duration))
    }

    @discardableResult
    public mutating func flush(
        throughGenerationID generationID: UInt64
    ) throws -> [PCMFrame] {
        if let latestGenerationID, generationID < latestGenerationID {
            throw PlaybackBufferError.staleGeneration
        }
        if latestGenerationID == nil || generationID > latestGenerationID! {
            latestGenerationID = generationID
        }
        if flushedThroughGenerationID == nil
            || generationID > flushedThroughGenerationID!
        {
            flushedThroughGenerationID = generationID
        }

        var retained: [QueuedFrame] = []
        var flushed: [PCMFrame] = []
        var retainedDuration: UInt64 = 0
        for queued in frames {
            if queued.frame.generationID <= generationID {
                flushed.append(queued.frame)
            } else {
                retained.append(queued)
                retainedDuration += queued.durationNanoseconds
            }
        }
        frames = retained
        queuedDurationNanoseconds = retainedDuration

        if currentStream?.generationID ?? UInt64.max <= generationID {
            currentStream = nil
            closedThroughUtteranceID = nil
            nextSequence = 0
        }
        return flushed
    }

    public mutating func markRendered(_ identity: PlaybackFrameIdentity) throws {
        guard let first = frames.first, first.frame.identity == identity else {
            throw PlaybackBufferError.renderOrderMismatch
        }
        frames.removeFirst()
        queuedDurationNanoseconds -= first.durationNanoseconds
    }

    private func validateGeneration(_ generationID: UInt64) throws {
        if let latestGenerationID, generationID < latestGenerationID {
            throw PlaybackBufferError.staleGeneration
        }
        if let flushedThroughGenerationID, generationID <= flushedThroughGenerationID {
            throw PlaybackBufferError.staleGeneration
        }
        if let latestGenerationID,
           generationID > latestGenerationID,
           !frames.isEmpty
        {
            throw PlaybackBufferError.overlappingGeneration
        }
    }

    private func frameDurationNanoseconds(_ frame: PCMFrame) throws -> UInt64 {
        let sampleFrames = UInt64(frame.bytes.count / frame.format.frameAlignmentBytes)
        let (numerator, overflow) = sampleFrames.multipliedReportingOverflow(by: 1_000_000_000)
        guard !overflow else {
            throw PlaybackBufferError.durationOverflow
        }
        let denominator = UInt64(frame.format.sampleRateHz)
        let (rounded, roundingOverflow) = numerator.addingReportingOverflow(denominator - 1)
        guard !roundingOverflow else {
            throw PlaybackBufferError.durationOverflow
        }
        return rounded / denominator
    }
}
