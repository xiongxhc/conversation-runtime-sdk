import Foundation
@testable import VoiceSidecarCore

func fixture(_ relativePath: String) throws -> URL {
    guard let root = ProcessInfo.processInfo.environment["VOICE_SIDECAR_FIXTURES_DIR"] else {
        throw TestSupportError.missingFixtureDirectory
    }
    return URL(fileURLWithPath: root, isDirectory: true).appendingPathComponent(relativePath)
}

enum TestSupportError: Error {
    case missingFixtureDirectory
    case unexpectedRead
}

actor ChunkedFrameReader: FrameByteReader {
    private var chunks: [Data]
    private(set) var requests: [Int] = []

    init(chunks: [Data]) {
        self.chunks = chunks
    }

    func read(upToCount count: Int) async throws -> Data {
        requests.append(count)
        guard !chunks.isEmpty else {
            return Data()
        }
        var chunk = chunks.removeFirst()
        if chunk.count > count {
            let remainder = chunk.dropFirst(count)
            chunk = Data(chunk.prefix(count))
            chunks.insert(Data(remainder), at: 0)
        }
        return chunk
    }
}

actor DelayedFrameReader: FrameByteReader {
    private var chunks: [Data]
    private let delayNanoseconds: UInt64

    init(chunks: [Data], delayNanoseconds: UInt64 = 10_000_000) {
        self.chunks = chunks
        self.delayNanoseconds = delayNanoseconds
    }

    func read(upToCount count: Int) async throws -> Data {
        try await Task.sleep(nanoseconds: delayNanoseconds)
        guard !chunks.isEmpty else {
            return Data()
        }
        var chunk = chunks.removeFirst()
        if chunk.count > count {
            let remainder = chunk.dropFirst(count)
            chunk = Data(chunk.prefix(count))
            chunks.insert(Data(remainder), at: 0)
        }
        return chunk
    }
}

actor BlockingFrameReader: FrameByteReader {
    private var chunks: [Data]
    private var released = false
    private var releaseContinuations: [CheckedContinuation<Void, Never>] = []

    init(chunks: [Data]) {
        self.chunks = chunks
    }

    func release() {
        released = true
        let continuations = releaseContinuations
        releaseContinuations.removeAll()
        for continuation in continuations {
            continuation.resume()
        }
    }

    func read(upToCount count: Int) async throws -> Data {
        if !released {
            await withCheckedContinuation { continuation in
                releaseContinuations.append(continuation)
            }
        }
        guard !chunks.isEmpty else {
            return Data()
        }
        var chunk = chunks.removeFirst()
        if chunk.count > count {
            let remainder = chunk.dropFirst(count)
            chunk = Data(chunk.prefix(count))
            chunks.insert(Data(remainder), at: 0)
        }
        return chunk
    }
}

actor SuspendedFrameReader: FrameByteReader {
    private var started = false
    private var startContinuations: [CheckedContinuation<Void, Never>] = []
    private var readContinuation: CheckedContinuation<Data, Error>?

    func waitUntilStarted() async {
        if started {
            return
        }
        await withCheckedContinuation { continuation in
            startContinuations.append(continuation)
        }
    }

    func read(upToCount _: Int) async throws -> Data {
        started = true
        let continuations = startContinuations
        startContinuations.removeAll()
        for continuation in continuations {
            continuation.resume()
        }
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                readContinuation = continuation
            }
        } onCancel: {
            Task {
                await self.cancelRead()
            }
        }
    }

    private func cancelRead() {
        readContinuation?.resume(throwing: CancellationError())
        readContinuation = nil
    }
}

actor RecordingByteWriter: FrameByteWriter {
    private(set) var writes: [Data] = []

    func write(_ data: Data) async throws {
        writes.append(data)
    }
}

actor SuspendingByteWriter: FrameByteWriter {
    private(set) var startedCount = 0
    private var startedContinuations: [
        (count: Int, continuation: CheckedContinuation<Void, Never>)
    ] = []
    private var firstWriteContinuation: CheckedContinuation<Void, Never>?

    func waitUntilStarted(_ count: Int) async {
        if startedCount >= count {
            return
        }
        await withCheckedContinuation { continuation in
            startedContinuations.append((count, continuation))
        }
    }

    func releaseFirstWrite() {
        firstWriteContinuation?.resume()
        firstWriteContinuation = nil
    }

    func write(_: Data) async throws {
        startedCount += 1
        let ready = startedContinuations.filter { startedCount >= $0.count }
        startedContinuations.removeAll { startedCount >= $0.count }
        for item in ready {
            item.continuation.resume()
        }
        if startedCount == 1 {
            await withCheckedContinuation { continuation in
                firstWriteContinuation = continuation
            }
        }
    }
}

actor FrameRecorder {
    private(set) var controlFrames: [ChildFrame] = []
    private(set) var mediaFrames: [ChildFrame] = []

    func recordControl(_ frame: ChildFrame) {
        controlFrames.append(frame)
    }

    func recordMedia(_ frame: ChildFrame) {
        mediaFrames.append(frame)
    }
}

actor CallLog {
    private(set) var entries: [String] = []

    func append(_ entry: String) {
        entries.append(entry)
    }

    func clear() {
        entries.removeAll()
    }
}

actor RecordingEventSink: SidecarEventSink {
    private let callLog: CallLog?
    private(set) var frames: [ChildFrame] = []

    init(callLog: CallLog? = nil) {
        self.callLog = callLog
    }

    func send(_ frame: ChildFrame) async throws {
        frames.append(frame)
        if let callLog {
            await callLog.append("event.\(frame.kind)")
        }
    }
}

actor RecordingAudioService: SidecarAudioService {
    private(set) var configurations: [SidecarConfiguration] = []
    private(set) var stopCount = 0
    var startFailure: SidecarServiceFailure?

    func start(configuration: SidecarConfiguration) async throws {
        if let startFailure {
            throw startFailure
        }
        configurations.append(configuration)
    }

    func stop() async {
        stopCount += 1
    }
}

actor RecordingRecognitionService: SidecarRecognitionService {
    private(set) var configurations: [SidecarConfiguration] = []
    private(set) var stopCount = 0
    var startFailure: SidecarServiceFailure?

    func start(configuration: SidecarConfiguration) async throws {
        if let startFailure {
            throw startFailure
        }
        configurations.append(configuration)
    }

    func stop() async {
        stopCount += 1
    }
}

actor RecordingPlaybackService: SidecarPlaybackService {
    private let callLog: CallLog?
    private(set) var frames: [PCMFrame] = []
    private(set) var flushedGenerations: [UInt64] = []
    private(set) var stopCount = 0
    var enqueueFailure: SidecarServiceFailure?
    var flushFailure: SidecarServiceFailure?

    init(callLog: CallLog? = nil) {
        self.callLog = callLog
    }

    func setEnqueueFailure(_ failure: SidecarServiceFailure) {
        enqueueFailure = failure
    }

    func enqueue(_ frame: PCMFrame) async throws {
        if let enqueueFailure {
            throw enqueueFailure
        }
        frames.append(frame)
        if let callLog {
            await callLog.append("playback.enqueue.\(frame.sequence)")
        }
    }

    func flush(throughGenerationID generationID: UInt64) async throws {
        if let flushFailure {
            throw flushFailure
        }
        flushedGenerations.append(generationID)
        if let callLog {
            await callLog.append("playback.flush.\(generationID)")
        }
    }

    func stop() async {
        stopCount += 1
    }
}

func pcmFrame(
    turnID: UInt64? = nil,
    generationID: UInt64 = 1,
    utteranceID: UInt64 = 1,
    sequence: UInt64 = 0,
    format: PCMFormat = PCMFormat(
        sampleRateHz: 24_000,
        channels: 1,
        sampleFormat: .signed16LittleEndian
    ),
    byteCount: Int = 960
) throws -> PCMFrame {
    try PCMFrame(
        turnID: turnID ?? generationID,
        generationID: generationID,
        utteranceID: utteranceID,
        sequence: sequence,
        format: format,
        bytes: Data(repeating: 0, count: byteCount)
    )
}

extension Data {
    mutating func appendBigEndian(_ value: UInt16) {
        append(UInt8(truncatingIfNeeded: value >> 8))
        append(UInt8(truncatingIfNeeded: value))
    }

    mutating func appendBigEndian(_ value: UInt32) {
        append(UInt8(truncatingIfNeeded: value >> 24))
        append(UInt8(truncatingIfNeeded: value >> 16))
        append(UInt8(truncatingIfNeeded: value >> 8))
        append(UInt8(truncatingIfNeeded: value))
    }

    mutating func appendBigEndian(_ value: UInt64) {
        append(UInt8(truncatingIfNeeded: value >> 56))
        append(UInt8(truncatingIfNeeded: value >> 48))
        append(UInt8(truncatingIfNeeded: value >> 40))
        append(UInt8(truncatingIfNeeded: value >> 32))
        append(UInt8(truncatingIfNeeded: value >> 24))
        append(UInt8(truncatingIfNeeded: value >> 16))
        append(UInt8(truncatingIfNeeded: value >> 8))
        append(UInt8(truncatingIfNeeded: value))
    }
}
