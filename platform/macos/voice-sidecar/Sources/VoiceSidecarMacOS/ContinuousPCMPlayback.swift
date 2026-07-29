@preconcurrency import AVFoundation
import VoiceSidecarCore

public actor ContinuousPCMPlayback: SidecarPlaybackService {
    public typealias RenderedHandler =
        @Sendable (
            PlaybackFrameIdentity
        ) async -> Void

    private let scheduler: any PCMPlaybackScheduling
    private var playbackBuffer = PlaybackBuffer()
    private var epoch: UInt64 = 0
    private var renderedHandler: RenderedHandler?

    public init(scheduler: any PCMPlaybackScheduling) {
        self.scheduler = scheduler
    }

    public func setRenderedHandler(_ handler: RenderedHandler?) {
        renderedHandler = handler
    }

    public func enqueue(_ frame: PCMFrame) async throws {
        var nextBuffer = playbackBuffer
        try nextBuffer.enqueue(frame)
        let audioBuffer = try PCMConversion.playbackBuffer(
            from: frame,
            convertedTo: scheduler.playbackFormat
        )
        let scheduledEpoch = epoch
        try scheduler.schedulePlayback(audioBuffer) { [weak self] in
            Task {
                await self?.rendered(
                    frame.identity,
                    scheduledEpoch: scheduledEpoch
                )
            }
        }
        playbackBuffer = nextBuffer
    }

    public func flush(
        throughGenerationID generationID: UInt64
    ) async throws {
        var nextBuffer = playbackBuffer
        _ = try nextBuffer.flush(throughGenerationID: generationID)
        epoch &+= 1
        playbackBuffer = nextBuffer
        scheduler.resetPlayback()
    }

    public func stop() async {
        epoch &+= 1
        playbackBuffer = PlaybackBuffer()
        renderedHandler = nil
        scheduler.resetPlayback()
    }

    private func rendered(
        _ identity: PlaybackFrameIdentity,
        scheduledEpoch: UInt64
    ) async {
        guard scheduledEpoch == epoch else {
            return
        }
        do {
            try playbackBuffer.markRendered(identity)
        } catch {
            return
        }
        await renderedHandler?(identity)
    }
}
