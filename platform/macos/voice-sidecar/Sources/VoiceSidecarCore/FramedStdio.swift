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
    private let fileHandle: FileHandle

    public init(fileHandle: FileHandle) {
        self.fileHandle = fileHandle
    }

    public func read(upToCount count: Int) async throws -> Data {
        try fileHandle.read(upToCount: count) ?? Data()
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
