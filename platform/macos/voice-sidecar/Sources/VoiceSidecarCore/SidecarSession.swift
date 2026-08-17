import Foundation

public struct SidecarConfiguration: Equatable, Sendable {
    public let sessionID: UInt64
    public let speechStartMilliseconds: UInt64
    public let finalSilenceMilliseconds: UInt64

    public init(
        sessionID: UInt64,
        speechStartMilliseconds: UInt64,
        finalSilenceMilliseconds: UInt64
    ) {
        self.sessionID = sessionID
        self.speechStartMilliseconds = speechStartMilliseconds
        self.finalSilenceMilliseconds = finalSilenceMilliseconds
    }
}

public struct SidecarServiceFailure: Error, Equatable, Sendable {
    public let stage: RuntimeStage
    public let code: SidecarFailureCode

    public init(stage: RuntimeStage, code: SidecarFailureCode) {
        self.stage = stage
        self.code = code
    }
}

public enum SidecarSessionError: Error, Equatable, Sendable {
    case invalidState
    case sessionMismatch
    case invalidSpeechStartThreshold
    case invalidFinalSilenceThreshold
    case unexpectedControl
    case expectedControlFrame
    case expectedAudioFrame
}

public protocol SidecarAudioService: Sendable {
    func start(configuration: SidecarConfiguration) async throws
    func pauseCapture() async throws
    func resumeCapture() async throws
    func stop() async
}

public protocol SidecarAudioDeviceStatusProviding: Sendable {
    func activeAudioDeviceStatus() async throws -> AudioDeviceStatus
}

public protocol SidecarRecognitionService: Sendable {
    func prepare(configuration: SidecarConfiguration) async throws
    func start(configuration: SidecarConfiguration) async throws
    func stop() async
}

public protocol SidecarPlaybackService: Sendable {
    func enqueue(_ frame: PCMFrame) async throws
    func flush(throughGenerationID generationID: UInt64) async throws
    func stop() async
}

public actor SidecarSession {
    private static let speechStartRange: ClosedRange<UInt64> = 100...1_000
    private static let finalSilenceRange: ClosedRange<UInt64> = 200...3_000

    private enum StablePhase: Equatable, Sendable {
        case ready
        case capturing
        case paused
    }

    private enum Phase: Equatable, Sendable {
        case awaitingSession
        case configuring
        case ready
        case starting
        case pausing
        case capturing
        case paused
        case resuming
        case flushing(StablePhase)
        case terminating
        case failing
        case terminated
    }

    private enum ServiceState: Sendable {
        case notAttempted
        case attempted
        case started
        case stopped
    }

    private enum RecognitionServiceState: Sendable {
        case notAttempted
        case prepareAttempted
        case prepared
        case startAttempted
        case started
        case stopped
    }

    private let audioService: any SidecarAudioService
    private let recognitionService: any SidecarRecognitionService
    private let playbackService: any SidecarPlaybackService
    private let eventSink: any SidecarEventSink

    private static let deferredMediaLimit = 32

    private var configuration: SidecarConfiguration?
    private var phase = Phase.awaitingSession
    private var audioState = ServiceState.notAttempted
    private var recognitionState = RecognitionServiceState.notAttempted
    private var playbackStopped = false
    private var cleanupStarted = false
    private var playbackBuffer = PlaybackBuffer()
    private var bargeInGate: BargeInGate?
    private var voiceActivityActive = false
    private var deferredMedia: [ChildFrame] = []
    private var stablePhaseWaiters: [CheckedContinuation<Void, Never>] = []

    public var isTerminated: Bool {
        phase == .terminated
    }

    public init(
        audioService: any SidecarAudioService,
        recognitionService: any SidecarRecognitionService,
        playbackService: any SidecarPlaybackService,
        eventSink: any SidecarEventSink
    ) {
        self.audioService = audioService
        self.recognitionService = recognitionService
        self.playbackService = playbackService
        self.eventSink = eventSink
    }

    public func handleControl(_ frame: ChildFrame) async throws {
        await awaitStablePhase()
        try requireAvailableOperation()
        guard let control = frame.control else {
            try await fail(
                SidecarSessionError.expectedControlFrame,
                fallbackSessionID: configuration?.sessionID ?? 0
            )
        }

        do {
            try await process(control)
        } catch {
            try await fail(error, fallbackSessionID: control.sessionID)
        }
    }

    public func handleMedia(_ frame: ChildFrame) async throws {
        // The media channel is independent of the control channel, so a
        // frame of an already-flushed generation can land while a flush or
        // barge-in still holds the session in a transient phase. Such a
        // frame is dead on arrival in every phase — drop it before phase
        // validation can escalate it into a session failure.
        if let audio = frame.audio,
            let configuration,
            audio.sessionID == configuration.sessionID,
            playbackBuffer.isExplicitlyStale(audio.frame.identity)
        {
            return
        }
        // A frame for a live generation can race the flush of an older one
        // (the next turn's audio starts before the flush acknowledgement
        // finishes). Park it until the flush restores a stable phase.
        if case .flushing = phase,
            let audio = frame.audio,
            let configuration,
            audio.sessionID == configuration.sessionID
        {
            guard deferredMedia.count < Self.deferredMediaLimit else {
                try await fail(
                    SidecarSessionError.invalidState,
                    fallbackSessionID: configuration.sessionID
                )
            }
            deferredMedia.append(frame)
            return
        }
        try requireAvailableOperation()
        guard let audio = frame.audio else {
            try await fail(
                SidecarSessionError.expectedAudioFrame,
                fallbackSessionID: configuration?.sessionID ?? 0
            )
        }

        do {
            let (configuration, playbackPhase) = try requirePlaybackAvailable()
            guard audio.sessionID == configuration.sessionID else {
                throw SidecarSessionError.sessionMismatch
            }
            // Frames travel on the media channel and flushes on the control
            // channel, so a frame for an already-flushed generation is an
            // expected race, not a protocol violation.
            if playbackBuffer.isExplicitlyStale(audio.frame.identity) {
                return
            }

            var nextBuffer = playbackBuffer
            try nextBuffer.enqueue(audio.frame)
            playbackBuffer = nextBuffer
            try await playbackService.enqueue(audio.frame)
            guard phase == playbackPhase,
                  playbackBuffer.contains(audio.frame.identity)
            else {
                return
            }
            try await eventSink.send(
                ChildFrame(
                    control: .playbackAccepted(
                        sessionID: configuration.sessionID,
                        turnID: audio.frame.turnID,
                        generationID: audio.frame.generationID,
                        utteranceID: audio.frame.utteranceID,
                        sequence: audio.frame.sequence
                    )
                )
            )
        } catch {
            try await fail(error, fallbackSessionID: audio.sessionID)
        }
    }

    public func playbackRendered(_ identity: PlaybackFrameIdentity) async throws {
        if playbackBuffer.isExplicitlyStale(identity) {
            return
        }
        do {
            await awaitStablePhase()
        try requireAvailableOperation()
            let (configuration, _) = try requirePlaybackAvailable()
            // Rendered acknowledgements cross several actors on their way
            // from the playback engine, so they can arrive out of order or
            // repeat after their frame already left the buffer. Playback is
            // strictly ordered, so resolve the prefix through the completed
            // frame and report each acknowledgement in order.
            for rendered in playbackBuffer.markRenderedThrough(identity) {
                try await eventSink.send(
                    ChildFrame(
                        control: .playbackRendered(
                            sessionID: configuration.sessionID,
                            turnID: rendered.turnID,
                            generationID: rendered.generationID,
                            utteranceID: rendered.utteranceID,
                            sequence: rendered.sequence
                        )
                    )
                )
            }
        } catch {
            try await fail(error, fallbackSessionID: configuration?.sessionID ?? 0)
        }
    }

    public func terminateFromServiceFailure(
        _ error: any Error,
        fallbackSessionID: UInt64
    ) async throws -> Never {
        try await fail(
            error,
            fallbackSessionID: fallbackSessionID
        )
    }

    public func recoverFromRecognitionFailure(
        _ failure: SidecarServiceFailure,
        fallbackSessionID: UInt64
    ) async throws {
        guard failure.stage == .speechRecognizer,
              failure.code == .recognitionFailed,
              phase == .capturing,
              let configuration
        else {
            throw SidecarSessionError.invalidState
        }
        recognitionState = .stopped
        await recognitionService.stop()
        guard phase == .capturing else {
            return
        }
        recognitionState = .prepareAttempted
        try await recognitionService.prepare(configuration: configuration)
        guard phase == .capturing else {
            return
        }
        recognitionState = .prepared
        recognitionState = .startAttempted
        try await recognitionService.start(configuration: configuration)
        guard phase == .capturing else {
            await recognitionService.stop()
            return
        }
        recognitionState = .started
        let sessionID = self.configuration?.sessionID ?? fallbackSessionID
        try await eventSink.send(
            ChildFrame(
                control: .failure(
                    sessionID: sessionID,
                    stage: failure.stage,
                    code: failure.code
                )
            )
        )
    }

    public func publishVoiceActivity(_ activity: VoiceActivity) async throws {
        await awaitStablePhase()
        try requireAvailableOperation()
        do {
            try await sendVoiceActivity(activity)
        } catch {
            try await fail(error, fallbackSessionID: configuration?.sessionID ?? 0)
        }
    }

    public func publishVoiceActivityFromRecognitionWorker(
        _ activity: VoiceActivity
    ) async throws {
        await awaitStablePhase()
        try requireAvailableOperation()
        try await sendVoiceActivity(activity)
    }

    public func publishRecognitionHypothesis(
        _ hypothesis: RecognitionHypothesis
    ) async throws {
        await awaitStablePhase()
        try requireAvailableOperation()
        do {
            try await sendRecognitionHypothesis(hypothesis)
        } catch {
            try await fail(error, fallbackSessionID: configuration?.sessionID ?? 0)
        }
    }

    public func publishRecognitionHypothesisFromWorker(
        _ hypothesis: RecognitionHypothesis
    ) async throws {
        await awaitStablePhase()
        try requireAvailableOperation()
        try await sendRecognitionHypothesis(hypothesis)
    }

    @discardableResult
    public func observeBargeIn(
        isSpeech: Bool,
        frameMilliseconds: UInt64,
        atMilliseconds: UInt64
    ) async throws -> Bool {
        await awaitStablePhase()
        try requireAvailableOperation()
        do {
            return try await processBargeIn(
                isSpeech: isSpeech,
                frameMilliseconds: frameMilliseconds,
                atMilliseconds: atMilliseconds
            )
        } catch {
            try await fail(error, fallbackSessionID: configuration?.sessionID ?? 0)
        }
    }

    @discardableResult
    public func observeBargeInFromRecognitionWorker(
        isSpeech: Bool,
        frameMilliseconds: UInt64,
        atMilliseconds: UInt64
    ) async throws -> Bool {
        await awaitStablePhase()
        try requireAvailableOperation()
        return try await processBargeIn(
            isSpeech: isSpeech,
            frameMilliseconds: frameMilliseconds,
            atMilliseconds: atMilliseconds
        )
    }

    public func observeCaptureDiscontinuityFromRecognitionWorker(
        atMilliseconds: UInt64
    ) async throws {
        await awaitStablePhase()
        try requireAvailableOperation()
        _ = try requireCapturing()
        if voiceActivityActive {
            try await sendVoiceActivity(
                .speechEnded(atMilliseconds: atMilliseconds)
            )
        }
        try await sendVoiceActivity(
            .captureDiscontinuity(atMilliseconds: atMilliseconds)
        )
        bargeInGate?.reset()
    }

    private func sendVoiceActivity(_ activity: VoiceActivity) async throws {
        let configuration = try requireCapturing()
        try await sendVoiceActivity(
            activity,
            sessionID: configuration.sessionID
        )
    }

    private func sendVoiceActivity(
        _ activity: VoiceActivity,
        sessionID: UInt64
    ) async throws {
        if case .speechStarted = activity, voiceActivityActive {
            return
        }
        try await eventSink.send(
            ChildFrame(
                control: .voiceActivity(
                    sessionID: sessionID,
                    activity: activity
                )
            )
        )
        switch activity {
        case .speechStarted, .speechContinued:
            voiceActivityActive = true
        case .speechEnded:
            voiceActivityActive = false
        case .captureDiscontinuity:
            return
        }
    }

    private func sendRecognitionHypothesis(
        _ hypothesis: RecognitionHypothesis
    ) async throws {
        let configuration = try requireCapturing()
        try await eventSink.send(
            ChildFrame(
                control: .transcriptHypothesis(
                    sessionID: configuration.sessionID,
                    hypothesis: hypothesis
                )
            )
        )
    }

    private func processBargeIn(
        isSpeech: Bool,
        frameMilliseconds: UInt64,
        atMilliseconds: UInt64
    ) async throws -> Bool {
        let configuration = try requireCapturing()
        guard playbackBuffer.isPlaybackActive,
            let generationID = playbackBuffer.activeGenerationID
        else {
            bargeInGate?.reset()
            return false
        }
        guard
            bargeInGate?.observe(
                isSpeech: isSpeech,
                frameMilliseconds: frameMilliseconds
            ) == true
        else {
            return false
        }

        let resumePhase = try reserveFlush(
            throughGenerationID: generationID
        )
        do {
            try await playbackService.flush(
                throughGenerationID: generationID
            )
            bargeInGate?.reset()
            try await sendVoiceActivity(
                .speechStarted(atMilliseconds: atMilliseconds),
                sessionID: configuration.sessionID
            )
        } catch {
            // The transient flush phase must settle before the error leaves
            // this actor, or inputs waiting on a stable phase hang forever.
            try await fail(error, fallbackSessionID: configuration.sessionID)
        }
        restoreAfterFlush(resumePhase)
        try await drainDeferredMedia()
        return true
    }

    private func drainDeferredMedia() async throws {
        while !deferredMedia.isEmpty {
            // A flush that starts mid-drain owns the remaining frames: its
            // completion drains again. Processing them here would just park
            // them again in a loop that never suspends.
            if case .flushing = phase {
                return
            }
            let frame = deferredMedia.removeFirst()
            try await handleMedia(frame)
        }
    }

    private func process(_ control: ChildControl) async throws {
        switch control {
        case let .startSession(
            sessionID,
            speechStartMilliseconds,
            finalSilenceMilliseconds
        ):
            guard phase == .awaitingSession, configuration == nil else {
                throw SidecarSessionError.invalidState
            }
            guard Self.speechStartRange.contains(speechStartMilliseconds) else {
                throw SidecarSessionError.invalidSpeechStartThreshold
            }
            guard Self.finalSilenceRange.contains(finalSilenceMilliseconds) else {
                throw SidecarSessionError.invalidFinalSilenceThreshold
            }
            let configuration = SidecarConfiguration(
                sessionID: sessionID,
                speechStartMilliseconds: speechStartMilliseconds,
                finalSilenceMilliseconds: finalSilenceMilliseconds
            )
            self.configuration = configuration
            bargeInGate = BargeInGate(
                speechStartMilliseconds: speechStartMilliseconds
            )
            phase = .configuring
            try await eventSink.send(ChildFrame(control: .ready(sessionID: sessionID)))
            phase = .ready

        case let .startCapture(sessionID, operationID):
            let configuration = try requireReady(sessionID: sessionID)
            phase = .starting
            recognitionState = .prepareAttempted
            try await recognitionService.prepare(configuration: configuration)
            recognitionState = .prepared
            audioState = .attempted
            try await audioService.start(configuration: configuration)
            audioState = .started
            recognitionState = .startAttempted
            try await recognitionService.start(configuration: configuration)
            recognitionState = .started
            try await emitAudioDeviceStatus(configuration)
            phase = .capturing
            try await eventSink.send(
                ChildFrame(
                    control: .captureStarted(
                        sessionID: configuration.sessionID,
                        operationID: operationID
                    )
                )
            )

        case let .pauseCapture(sessionID, operationID):
            let configuration = try requireConfigured(sessionID: sessionID)
            guard phase == .capturing else {
                throw SidecarSessionError.invalidState
            }
            phase = .pausing
            recognitionState = .stopped
            await recognitionService.stop()
            try await audioService.pauseCapture()
            try await eventSink.send(
                ChildFrame(
                    control: .capturePaused(
                        sessionID: configuration.sessionID,
                        operationID: operationID
                    )
                )
            )
            phase = .paused

        case let .resumeCapture(sessionID, operationID):
            let configuration = try requireConfigured(sessionID: sessionID)
            guard phase == .paused else {
                throw SidecarSessionError.invalidState
            }
            phase = .resuming
            recognitionState = .prepareAttempted
            try await recognitionService.prepare(configuration: configuration)
            recognitionState = .prepared
            try await audioService.resumeCapture()
            recognitionState = .startAttempted
            try await recognitionService.start(configuration: configuration)
            recognitionState = .started
            // The default device can change while capture is paused, so the
            // resumed session reports the devices it actually came back on.
            try await emitAudioDeviceStatus(configuration)
            try await eventSink.send(
                ChildFrame(
                    control: .captureResumed(
                        sessionID: configuration.sessionID,
                        operationID: operationID
                    )
                )
            )
            phase = .capturing

        case let .flushGeneration(sessionID, generationID, operationID):
            let configuration = try requireConfigured(sessionID: sessionID)
            // A parent flush can lose the race against a local barge-in flush
            // that already advanced the buffer to a newer generation. The
            // requested generation is gone either way, so acknowledge without
            // touching the newer generation's playback.
            let resumePhase: StablePhase?
            do {
                resumePhase = try reserveFlush(
                    throughGenerationID: generationID
                )
            } catch PlaybackBufferError.staleGeneration {
                resumePhase = nil
            }
            if resumePhase != nil {
                try await playbackService.flush(
                    throughGenerationID: generationID
                )
                bargeInGate?.reset()
            }
            try await eventSink.send(
                ChildFrame(
                    control: .playbackFlushed(
                        sessionID: configuration.sessionID,
                        generationID: generationID,
                        operationID: operationID
                    )
                )
            )
            if let resumePhase {
                restoreAfterFlush(resumePhase)
            }
            try await drainDeferredMedia()

        case let .shutdown(sessionID):
            let configuration = try requireConfigured(sessionID: sessionID)
            guard phase == .ready || phase == .capturing || phase == .paused else {
                throw SidecarSessionError.invalidState
            }
            phase = .terminating
            await cleanupServices()
            try await eventSink.send(
                ChildFrame(control: .shutdownComplete(sessionID: configuration.sessionID))
            )
            phase = .terminated

        case .ready,
             .audioDeviceStatus,
             .captureStarted,
             .capturePaused,
             .captureResumed,
             .voiceActivity,
             .transcriptHypothesis,
             .playbackAccepted,
             .playbackRendered,
             .playbackFlushed,
             .failure,
             .shutdownComplete:
            throw SidecarSessionError.unexpectedControl
        }
    }

    // Device labels are informational. A device that cannot name itself
    // (aggregate and virtual devices often cannot) must never fail a capture
    // that is otherwise running, so the status frame is simply skipped.
    private func emitAudioDeviceStatus(
        _ configuration: SidecarConfiguration
    ) async throws {
        guard let statusProvider = audioService as? any SidecarAudioDeviceStatusProviding,
              let devices = try? await statusProvider.activeAudioDeviceStatus(),
              !devices.inputLabel.isEmpty,
              !devices.outputLabel.isEmpty
        else {
            return
        }
        try await eventSink.send(
            ChildFrame(
                control: .audioDeviceStatus(
                    sessionID: configuration.sessionID,
                    inputLabel: devices.inputLabel,
                    outputLabel: devices.outputLabel
                )
            )
        )
    }

    private func reserveFlush(
        throughGenerationID generationID: UInt64
    ) throws -> StablePhase {
        let resumePhase: StablePhase
        switch phase {
        case .ready:
            resumePhase = .ready
        case .capturing:
            resumePhase = .capturing
        case .paused:
            resumePhase = .paused
        default:
            throw SidecarSessionError.invalidState
        }
        var nextBuffer = playbackBuffer
        _ = try nextBuffer.flush(throughGenerationID: generationID)
        playbackBuffer = nextBuffer
        phase = .flushing(resumePhase)
        return resumePhase
    }

    private func restoreAfterFlush(_ resumePhase: StablePhase) {
        guard phase == .flushing(resumePhase) else {
            return
        }
        switch resumePhase {
        case .ready:
            phase = .ready
        case .capturing:
            phase = .capturing
        case .paused:
            phase = .paused
        }
        resumeStablePhaseWaiters()
    }

    // A flush holds the session in a transient phase across suspension
    // points, and asynchronous inputs — controls, rendered acknowledgements,
    // recognition events — regularly race it. They wait the flush out here
    // instead of tripping phase validation into a session failure.
    private func awaitStablePhase() async {
        while case .flushing = phase {
            await withCheckedContinuation { continuation in
                stablePhaseWaiters.append(continuation)
            }
        }
    }

    private func resumeStablePhaseWaiters() {
        let waiters = stablePhaseWaiters
        stablePhaseWaiters.removeAll()
        for waiter in waiters {
            waiter.resume()
        }
    }

    private func requireConfigured(
        sessionID: UInt64
    ) throws -> SidecarConfiguration {
        guard let configuration else {
            throw SidecarSessionError.invalidState
        }
        guard configuration.sessionID == sessionID else {
            throw SidecarSessionError.sessionMismatch
        }
        return configuration
    }

    private func requireReady(
        sessionID: UInt64
    ) throws -> SidecarConfiguration {
        let configuration = try requireConfigured(sessionID: sessionID)
        guard phase == .ready else {
            throw SidecarSessionError.invalidState
        }
        return configuration
    }

    private func requireCapturing() throws -> SidecarConfiguration {
        guard phase == .capturing, let configuration else {
            throw SidecarSessionError.invalidState
        }
        return configuration
    }

    private func requirePlaybackAvailable() throws -> (
        SidecarConfiguration,
        Phase
    ) {
        guard let configuration else {
            throw SidecarSessionError.invalidState
        }
        switch phase {
        case .capturing, .paused:
            return (configuration, phase)
        default:
            throw SidecarSessionError.invalidState
        }
    }

    private func requireAvailableOperation() throws {
        switch phase {
        case .awaitingSession, .ready, .capturing, .paused:
            return
        case .configuring,
             .starting,
             .pausing,
             .flushing,
             .resuming,
             .terminating,
             .failing,
             .terminated:
            throw SidecarSessionError.invalidState
        }
    }

    private func fail(
        _ error: any Error,
        fallbackSessionID: UInt64
    ) async throws -> Never {
        guard phase != .failing,
              phase != .terminating,
              phase != .terminated
        else {
            throw error
        }
        let sessionID = configuration?.sessionID ?? fallbackSessionID
        let failure: SidecarServiceFailure
        if let serviceFailure = error as? SidecarServiceFailure {
            failure = serviceFailure
        } else if error is PlaybackBufferError || error is ChildProtocolError {
            failure = SidecarServiceFailure(
                stage: .voiceSidecar,
                code: .malformedFrame
            )
        } else if error is SidecarSessionError {
            failure = SidecarServiceFailure(stage: .voiceSidecar, code: .invalidState)
        } else {
            failure = SidecarServiceFailure(stage: .voiceSidecar, code: .internal)
        }
        // Temporary diagnostic for failure triage; active only when
        // VOICE_SIDECAR_DIAG_PATH points at an existing file. The underlying
        // error never leaves the process through the protocol, which reports
        // only the typed stage and code.
        if let path = ProcessInfo.processInfo.environment["VOICE_SIDECAR_DIAG_PATH"],
            let handle = FileHandle(forWritingAtPath: path)
        {
            let line = "fail phase=\(phase) stage=\(failure.stage) code=\(failure.code) error=\(error)\n"
            if let data = line.data(using: .utf8) {
                handle.seekToEndOfFile()
                handle.write(data)
            }
            try? handle.close()
        }

        phase = .failing
        resumeStablePhaseWaiters()
        await cleanupServices()
        try await eventSink.send(
            ChildFrame(
                control: .failure(
                    sessionID: sessionID,
                    stage: failure.stage,
                    code: failure.code
                )
            )
        )
        throw error
    }

    private func cleanupServices() async {
        guard !cleanupStarted else {
            return
        }
        cleanupStarted = true
        deferredMedia.removeAll()

        if recognitionState == .startAttempted || recognitionState == .started {
            recognitionState = .stopped
            await recognitionService.stop()
        }
        if audioState == .attempted || audioState == .started {
            audioState = .stopped
            await audioService.stop()
        }
        if recognitionState == .prepareAttempted || recognitionState == .prepared {
            recognitionState = .stopped
            await recognitionService.stop()
        }
        if !playbackStopped {
            playbackStopped = true
            await playbackService.stop()
        }
    }
}
