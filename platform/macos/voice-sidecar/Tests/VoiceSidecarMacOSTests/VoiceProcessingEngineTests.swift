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

@Test
func captureControlsRejectBeforeTheAudioGraphStarts() async {
    let engine = VoiceProcessingEngine(
        permissionProvider: AuthorizedMicrophonePermissionProvider()
    )
    let expected = SidecarServiceFailure(
        stage: .audioCapture,
        code: .invalidState
    )

    do {
        try await engine.pauseCapture()
        Issue.record("pause unexpectedly succeeded before graph start")
    } catch let failure as SidecarServiceFailure {
        #expect(failure == expected)
    } catch {
        Issue.record("unexpected pause error \(error)")
    }

    do {
        try await engine.resumeCapture()
        Issue.record("resume unexpectedly succeeded before graph start")
    } catch let failure as SidecarServiceFailure {
        #expect(failure == expected)
    } catch {
        Issue.record("unexpected resume error \(error)")
    }
}

@Test
func capturePumpStopWaitsForInFlightProcessing() async throws {
    let format = try #require(
        AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: 48_000,
            channels: 1,
            interleaved: false
        )
    )
    let buffer = try #require(
        AVAudioPCMBuffer(pcmFormat: format, frameCapacity: 4_800)
    )
    buffer.frameLength = 4_800
    let ring = try VoiceProcessingEngine.makeCaptureRing(format: format)
    let processingStarted = DispatchSemaphore(value: 0)
    let releaseProcessing = DispatchSemaphore(value: 0)
    let pump = CaptureBufferPump(
        ring: ring,
        eventHandler: { event in
            guard case .buffer = event else {
                return
            }
            processingStarted.signal()
            releaseProcessing.wait()
        },
        faultHandler: { _ in }
    )
    #expect(pump.enqueue(buffer) == .enqueued)
    let processingObserved = await Task.detached {
        waitForSemaphore(processingStarted, timeout: .now() + 1)
    }.value
    #expect(processingObserved == .success)

    let stopReturned = DispatchSemaphore(value: 0)
    let stop = Task.detached {
        pump.stop()
        stopReturned.signal()
    }
    let stopWasBlocked = await Task.detached {
        waitForSemaphore(stopReturned, timeout: .now() + 0.02)
    }.value
    #expect(stopWasBlocked == .timedOut)

    releaseProcessing.signal()
    await stop.value
    let stopCompleted = await Task.detached {
        waitForSemaphore(stopReturned, timeout: .now() + 1)
    }.value
    #expect(stopCompleted == .success)
}

private func waitForSemaphore(
    _ semaphore: DispatchSemaphore,
    timeout: DispatchTime
) -> DispatchTimeoutResult {
    semaphore.wait(timeout: timeout)
}

private struct AuthorizedMicrophonePermissionProvider:
    MicrophonePermissionProviding
{
    func authorizationStatus() -> MicrophoneAuthorization {
        .authorized
    }

    func requestAccess() async -> Bool {
        true
    }
}
