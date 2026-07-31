import Darwin
import Dispatch
import Foundation

public protocol FrameByteReader: Sendable {
    func read(upToCount count: Int) async throws -> Data
}

public protocol FrameByteWriter: Sendable {
    func write(_ data: Data) async throws
}

public protocol SidecarEventSink: Sendable {
    func send(_ frame: ChildFrame) async throws
}

public enum FrameLoopDirective: Sendable {
    case `continue`
    case stop
}

public enum FramedStdioError: Error, Equatable, Sendable {
    case controlChannelReceivedAudio
    case mediaChannelReceivedControl
    case unexpectedControlEOF
    case descriptorDuplicateFailed(Int32)
    case descriptorReadFailed(Int32)
}

public actor SerializedFrameWriter: SidecarEventSink {
    private let writer: any FrameByteWriter
    private var writeInProgress = false
    private var writeWaiters: [CheckedContinuation<Void, Never>] = []

    public init(writer: any FrameByteWriter) {
        self.writer = writer
    }

    public func send(_ frame: ChildFrame) async throws {
        await acquireWriteSlot()
        defer {
            releaseWriteSlot()
        }
        try await writer.write(ChildProtocol.encode(frame))
    }

    private func acquireWriteSlot() async {
        if !writeInProgress {
            writeInProgress = true
            return
        }
        await withCheckedContinuation { continuation in
            writeWaiters.append(continuation)
        }
    }

    private func releaseWriteSlot() {
        guard !writeWaiters.isEmpty else {
            writeInProgress = false
            return
        }
        writeWaiters.removeFirst().resume()
    }
}

public struct FramedStdio: Sendable {
    public typealias Handler = @Sendable (ChildFrame) async throws -> FrameLoopDirective

    private let controlReader: any FrameByteReader
    private let mediaReader: any FrameByteReader

    public init(
        controlReader: any FrameByteReader,
        mediaReader: any FrameByteReader
    ) {
        self.controlReader = controlReader
        self.mediaReader = mediaReader
    }

    public func run(
        onControl: @escaping Handler,
        onMedia: @escaping Handler
    ) async throws {
        try await withThrowingTaskGroup(of: ChannelCompletion.self) { group in
            group.addTask {
                try await Self.readLoop(
                    from: controlReader,
                    expectsAudio: false,
                    handler: onControl
                )
            }
            group.addTask {
                try await Self.readLoop(
                    from: mediaReader,
                    expectsAudio: true,
                    handler: onMedia
                )
            }

            while let completion = try await group.next() {
                if completion == .stopRequested {
                    group.cancelAll()
                    return
                }
            }
        }
    }

    public static func readFrame(
        from reader: any FrameByteReader
    ) async throws -> ChildFrame? {
        guard let headerData = try await readExactly(
            ChildProtocol.headerBytes,
            from: reader,
            allowEmptyEOF: true,
            requiredTotal: ChildProtocol.headerBytes,
            availableBase: 0
        ) else {
            return nil
        }

        let header = try ChildProtocol.decodeHeader(headerData)
        let payload = try await readExactly(
            header.payloadLength,
            from: reader,
            allowEmptyEOF: header.payloadLength == 0,
            requiredTotal: ChildProtocol.headerBytes + header.payloadLength,
            availableBase: ChildProtocol.headerBytes
        ) ?? Data()
        var frameData = headerData
        frameData.append(payload)
        return try ChildProtocol.decode(frameData)
    }

    private static func readLoop(
        from reader: any FrameByteReader,
        expectsAudio: Bool,
        handler: @escaping Handler
    ) async throws -> ChannelCompletion {
        while !Task.isCancelled {
            guard let frame = try await readFrame(from: reader) else {
                if expectsAudio {
                    return .mediaEOF
                }
                throw FramedStdioError.unexpectedControlEOF
            }
            if expectsAudio, frame.kind != .audioFrame {
                throw FramedStdioError.mediaChannelReceivedControl
            }
            if !expectsAudio, frame.kind == .audioFrame {
                throw FramedStdioError.controlChannelReceivedAudio
            }
            if try await handler(frame) == .stop {
                return .stopRequested
            }
        }
        return .stopRequested
    }

    private static func readExactly(
        _ count: Int,
        from reader: any FrameByteReader,
        allowEmptyEOF: Bool,
        requiredTotal: Int,
        availableBase: Int
    ) async throws -> Data? {
        if count == 0 {
            return Data()
        }

        var data = Data()
        data.reserveCapacity(count)
        while data.count < count {
            try Task.checkCancellation()
            let chunk = try await reader.read(upToCount: count - data.count)
            if chunk.isEmpty {
                if data.isEmpty, allowEmptyEOF {
                    return nil
                }
                throw ChildProtocolError.truncatedFrame(
                    required: requiredTotal,
                    available: availableBase + data.count
                )
            }
            data.append(chunk)
        }
        return data
    }
}

private enum ChannelCompletion: Equatable, Sendable {
    case mediaEOF
    case stopRequested
}

public final class FileHandleFrameReader: FrameByteReader, @unchecked Sendable {
    private let descriptor: Int32
    private let setupError: FramedStdioError?

    public init(fileHandle: FileHandle) {
        let duplicated = dup(fileHandle.fileDescriptor)
        guard duplicated >= 0 else {
            descriptor = -1
            setupError = .descriptorDuplicateFailed(errno)
            return
        }
        descriptor = duplicated
        setupError = nil
    }

    deinit {
        if descriptor >= 0 {
            close(descriptor)
        }
    }

    public func read(upToCount count: Int) async throws -> Data {
        if let setupError {
            throw setupError
        }
        return try await DescriptorReadOperation(
            descriptor: descriptor,
            count: count
        ).run()
    }
}

public final class FileHandleFrameWriter: FrameByteWriter, @unchecked Sendable {
    private let fileHandle: FileHandle

    public init(fileHandle: FileHandle) {
        self.fileHandle = fileHandle
    }

    public func write(_ data: Data) async throws {
        try fileHandle.write(contentsOf: data)
    }
}

private final class DescriptorReadOperation: @unchecked Sendable {
    private let descriptor: Int32
    private let count: Int
    private let lock = NSLock()
    private var source: DispatchSourceRead?
    private var continuation: CheckedContinuation<Data, Error>?
    private var isCancelled = false
    private var isFinished = false

    init(descriptor: Int32, count: Int) {
        self.descriptor = descriptor
        self.count = count
    }

    func run() async throws -> Data {
        try Task.checkCancellation()
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                start(continuation)
            }
        } onCancel: {
            cancel()
        }
    }

    private func start(
        _ continuation: CheckedContinuation<Data, Error>
    ) {
        lock.lock()
        guard !isCancelled else {
            lock.unlock()
            continuation.resume(throwing: CancellationError())
            return
        }

        self.continuation = continuation
        let source = DispatchSource.makeReadSource(
            fileDescriptor: descriptor,
            queue: .global(qos: .userInitiated)
        )
        self.source = source
        source.setEventHandler { [weak self] in
            self?.readAvailableBytes()
        }
        source.resume()
        lock.unlock()
    }

    private func cancel() {
        lock.lock()
        isCancelled = true
        lock.unlock()
        finish(.failure(CancellationError()))
    }

    private func readAvailableBytes() {
        var bytes = [UInt8](repeating: 0, count: count)
        let readCount = Darwin.read(descriptor, &bytes, count)
        if readCount > 0 {
            finish(.success(Data(bytes.prefix(Int(readCount)))))
            return
        }
        if readCount == 0 {
            finish(.success(Data()))
            return
        }
        if errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR {
            return
        }
        finish(.failure(FramedStdioError.descriptorReadFailed(errno)))
    }

    private func finish(_ result: Result<Data, Error>) {
        lock.lock()
        guard !isFinished else {
            lock.unlock()
            return
        }
        isFinished = true
        let continuation = continuation
        self.continuation = nil
        let source = source
        self.source = nil
        lock.unlock()

        source?.cancel()
        continuation?.resume(with: result)
    }
}
