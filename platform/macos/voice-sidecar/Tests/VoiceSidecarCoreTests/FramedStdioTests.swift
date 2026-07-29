import Darwin
import Foundation
import Testing
@testable import VoiceSidecarCore

@Test
func partialReadsAssembleOneExactFrame() async throws {
    let data = try Data(contentsOf: fixture("control/start-session.bin"))
    let reader = ChunkedFrameReader(chunks: data.map { Data([$0]) })

    let frame = try #require(try await FramedStdio.readFrame(from: reader))

    #expect(frame.kind == .startSession)
    #expect(try ChildProtocol.encode(frame) == data)
}

@Test
func oversizedHeaderIsRejectedBeforePayloadRead() async {
    let data = try! Data(contentsOf: fixture("invalid/oversized-header.bin"))
    let reader = ChunkedFrameReader(chunks: [data])

    do {
        _ = try await FramedStdio.readFrame(from: reader)
        Issue.record("expected oversized payload rejection")
    } catch let error as ChildProtocolError {
        #expect(
            error
                == .payloadTooLarge(
                    kind: .startSession,
                    declared: 65_537,
                    maximum: 65_536
                )
        )
    } catch {
        Issue.record("unexpected error \(error)")
    }

    #expect(await reader.requests == [8])
}

@Test
func truncatedReaderEOFIsTyped() async {
    let data = try! Data(contentsOf: fixture("invalid/truncated-control.bin"))
    let reader = ChunkedFrameReader(chunks: [data])

    do {
        _ = try await FramedStdio.readFrame(from: reader)
        Issue.record("expected truncated frame")
    } catch let error as ChildProtocolError {
        guard case let .truncatedFrame(required, available) = error else {
            Issue.record("unexpected error \(error)")
            return
        }
        #expect(required == 69)
        #expect(available == data.count)
    } catch {
        Issue.record("unexpected error \(error)")
    }
}

@Test
func shortPayloadEOFReportsWholeFrameRequirement() async {
    var data = Data()
    data.appendBigEndian(UInt16(1))
    data.appendBigEndian(ChildFrameKind.startCapture.rawValue)
    data.appendBigEndian(UInt32(3))
    data.append(Data([0x7B, 0x7D]))
    let reader = ChunkedFrameReader(chunks: [data])

    do {
        _ = try await FramedStdio.readFrame(from: reader)
        Issue.record("expected truncated frame")
    } catch let error as ChildProtocolError {
        #expect(error == .truncatedFrame(required: 11, available: 10))
    } catch {
        Issue.record("unexpected error \(error)")
    }
}

@Test
func flushControlIsHandledWhileMediaReaderRemainsSuspended() async throws {
    let flush = ChildFrame(
        control: .flushGeneration(sessionID: 7, generationID: 9, operationID: 11)
    )
    let controlReader = BlockingFrameReader(chunks: [try ChildProtocol.encode(flush)])
    let mediaReader = SuspendedFrameReader()
    let recorder = FrameRecorder()
    let stdio = FramedStdio(controlReader: controlReader, mediaReader: mediaReader)

    let task = Task {
        try await stdio.run(
            onControl: { frame in
                await recorder.recordControl(frame)
                return .stop
            },
            onMedia: { frame in
                await recorder.recordMedia(frame)
                return .continue
            }
        )
    }

    await mediaReader.waitUntilStarted()
    await controlReader.release()
    try await task.value

    #expect(await recorder.controlFrames == [flush])
    #expect(await recorder.mediaFrames.isEmpty)
}

@Test
func mediaEOFDoesNotCancelTheIndependentControlReader() async throws {
    let flush = ChildFrame(
        control: .flushGeneration(sessionID: 7, generationID: 9, operationID: 11)
    )
    let controlReader = DelayedFrameReader(chunks: [try ChildProtocol.encode(flush)])
    let mediaReader = ChunkedFrameReader(chunks: [])
    let recorder = FrameRecorder()
    let stdio = FramedStdio(controlReader: controlReader, mediaReader: mediaReader)

    try await stdio.run(
        onControl: { frame in
            await recorder.recordControl(frame)
            return .stop
        },
        onMedia: { frame in
            await recorder.recordMedia(frame)
            return .continue
        }
    )

    #expect(await recorder.controlFrames == [flush])
}

@Test
func shutdownCancelsIdleDescriptorReadWithoutClosingItsOpenPeer() async throws {
    var controlDescriptors = [Int32](repeating: -1, count: 2)
    var mediaDescriptors = [Int32](repeating: -1, count: 2)
    #expect(socketpair(AF_UNIX, SOCK_STREAM, 0, &controlDescriptors) == 0)
    #expect(socketpair(AF_UNIX, SOCK_STREAM, 0, &mediaDescriptors) == 0)

    let controlInput = FileHandle(
        fileDescriptor: controlDescriptors[0],
        closeOnDealloc: true
    )
    let controlPeer = FileHandle(
        fileDescriptor: controlDescriptors[1],
        closeOnDealloc: true
    )
    let mediaInput = FileHandle(
        fileDescriptor: mediaDescriptors[0],
        closeOnDealloc: true
    )
    let mediaPeer = FileHandle(
        fileDescriptor: mediaDescriptors[1],
        closeOnDealloc: true
    )
    defer {
        try? controlInput.close()
        try? controlPeer.close()
        try? mediaInput.close()
        try? mediaPeer.close()
    }

    let events = RecordingEventSink()
    let session = SidecarSession(
        audioService: RecordingAudioService(),
        recognitionService: RecordingRecognitionService(),
        playbackService: RecordingPlaybackService(),
        eventSink: events
    )
    let stdio = FramedStdio(
        controlReader: FileHandleFrameReader(fileHandle: controlInput),
        mediaReader: FileHandleFrameReader(fileHandle: mediaInput)
    )
    let completion = CompletionFlag()
    let task = Task {
        do {
            try await stdio.run(
                onControl: { frame in
                    try await session.handleControl(frame)
                    return await session.isTerminated ? .stop : .continue
                },
                onMedia: { frame in
                    try await session.handleMedia(frame)
                    return .continue
                }
            )
            await completion.complete()
        } catch {
            await completion.complete()
            throw error
        }
    }

    var control = try ChildProtocol.encode(
        ChildFrame(
            control: .startSession(
                sessionID: 7,
                speechStartMilliseconds: 200,
                finalSilenceMilliseconds: 600
            )
        )
    )
    control.append(
        try ChildProtocol.encode(
            ChildFrame(control: .shutdown(sessionID: 7))
        )
    )
    try controlPeer.write(contentsOf: control)

    let completedBeforeForcedClose = await waitUntil {
        await completion.isComplete
    }
    if !completedBeforeForcedClose {
        _ = shutdown(mediaInput.fileDescriptor, SHUT_RDWR)
        try? mediaInput.close()
    }
    try await task.value

    #expect(completedBeforeForcedClose)
    #expect(
        await events.frames
            == [
                ChildFrame(control: .ready(sessionID: 7)),
                ChildFrame(control: .shutdownComplete(sessionID: 7)),
            ]
    )
    #expect(fcntl(controlInput.fileDescriptor, F_GETFD) != -1)
    #expect(fcntl(mediaPeer.fileDescriptor, F_GETFD) != -1)

    if completedBeforeForcedClose {
        var noSignal = Int32(1)
        #expect(
            setsockopt(
                mediaPeer.fileDescriptor,
                SOL_SOCKET,
                SO_NOSIGPIPE,
                &noSignal,
                socklen_t(MemoryLayout.size(ofValue: noSignal))
            ) == 0
        )
        let sent = Data([0]).withUnsafeBytes {
            send(mediaPeer.fileDescriptor, $0.baseAddress, $0.count, 0)
        }
        #expect(sent == 1)
    }
}

@Test
func serializedWriterNeverInterleavesConcurrentFrames() async throws {
    let byteWriter = RecordingByteWriter()
    let writer = SerializedFrameWriter(writer: byteWriter)

    await withTaskGroup(of: Void.self) { group in
        for sessionID in 1...20 {
            group.addTask {
                try? await writer.send(
                    ChildFrame(control: .ready(sessionID: UInt64(sessionID)))
                )
            }
        }
    }

    let writes = await byteWriter.writes
    #expect(writes.count == 20)
    for data in writes {
        #expect((try? ChildProtocol.decode(data).kind) == .ready)
    }
}

@Test
func serializedWriterDoesNotReenterWhileAWriteIsSuspended() async {
    let byteWriter = SuspendingByteWriter()
    let writer = SerializedFrameWriter(writer: byteWriter)
    let first = Task {
        try? await writer.send(ChildFrame(control: .ready(sessionID: 1)))
    }
    await byteWriter.waitUntilStarted(1)

    let second = Task {
        try? await writer.send(ChildFrame(control: .ready(sessionID: 2)))
    }
    for _ in 0..<10 {
        await Task.yield()
    }

    #expect(await byteWriter.startedCount == 1)
    await byteWriter.releaseFirstWrite()
    _ = await first.value
    _ = await second.value
    #expect(await byteWriter.startedCount == 2)
}
