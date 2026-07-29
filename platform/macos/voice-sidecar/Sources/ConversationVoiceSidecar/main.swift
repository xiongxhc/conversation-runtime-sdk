import Darwin
import Foundation
import VoiceSidecarCore

private actor ProtocolOnlyAudioService: SidecarAudioService {
    func start(configuration _: SidecarConfiguration) async throws {}
    func stop() async {}
}

private actor ProtocolOnlyRecognitionService: SidecarRecognitionService {
    func start(configuration _: SidecarConfiguration) async throws {}
    func stop() async {}
}

private actor ProtocolOnlyPlaybackService: SidecarPlaybackService {
    func enqueue(_: PCMFrame) async throws {}
    func flush(throughGenerationID _: UInt64) async throws {}
    func stop() async {}
}

let eventWriter = SerializedFrameWriter(
    writer: FileHandleFrameWriter(fileHandle: .standardOutput)
)
let session = SidecarSession(
    audioService: ProtocolOnlyAudioService(),
    recognitionService: ProtocolOnlyRecognitionService(),
    playbackService: ProtocolOnlyPlaybackService(),
    eventSink: eventWriter
)
let stdio = FramedStdio(
    controlReader: FileHandleFrameReader(fileHandle: .standardInput),
    mediaReader: FileHandleFrameReader(
        fileHandle: FileHandle(fileDescriptor: 3, closeOnDealloc: false)
    )
)

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
} catch {
    try? FileHandle.standardError.write(contentsOf: Data("voice sidecar failed\n".utf8))
    exit(EXIT_FAILURE)
}
