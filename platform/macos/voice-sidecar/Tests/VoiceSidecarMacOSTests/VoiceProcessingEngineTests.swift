import AVFoundation
import Testing

@testable import VoiceSidecarCore
@testable import VoiceSidecarMacOS

@Test
func voiceProcessingPlaybackGraphPresentsMonoToTheOutputNode() throws {
    let engine = AVAudioEngine()
    let player = AVAudioPlayerNode()

    let playbackFormat = try VoiceProcessingEngine.configurePlaybackGraph(
        engine: engine,
        player: player,
        sampleRate: 48_000
    )

    #expect(playbackFormat.sampleRate == 48_000)
    #expect(playbackFormat.channelCount == 1)
    #expect(engine.mainMixerNode.outputFormat(forBus: 0).channelCount == 1)
    #expect(
        engine.outputConnectionPoints(
            for: engine.mainMixerNode,
            outputBus: 0
        ).contains { $0.node === engine.outputNode }
    )
}

@Test
func voiceProcessingCaptureRingAcceptsOneHundredMillisecondBuffers() throws {
    let format = try #require(
        AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: 48_000,
            channels: 1,
            interleaved: false
        )
    )
    let buffer = try #require(
        AVAudioPCMBuffer(
            pcmFormat: format,
            frameCapacity: 4_800
        )
    )
    buffer.frameLength = 4_800

    let ring = try VoiceProcessingEngine.makeCaptureRing(format: format)

    #expect(ring.copyFromTap(buffer) == .enqueued)
}

@Test
func invalidVoiceProcessingOutputFormatReportsAudioOutputFailure() {
    let engine = AVAudioEngine()
    let player = AVAudioPlayerNode()

    #expect(
        throws: SidecarServiceFailure(
            stage: .audioOutput,
            code: .playbackFailed
        )
    ) {
        _ = try VoiceProcessingEngine.configurePlaybackGraph(
            engine: engine,
            player: player,
            sampleRate: 0
        )
    }
}

@Test
func voiceProcessingStartupPreservesTypedFailureStage() {
    let failure = SidecarServiceFailure(
        stage: .audioOutput,
        code: .playbackFailed
    )

    #expect(
        VoiceProcessingEngine.startupFailure(from: failure) == failure
    )
}
