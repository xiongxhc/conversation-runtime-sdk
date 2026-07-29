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
    func stop() async
}

public protocol SidecarRecognitionService: Sendable {
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
    }

    private enum Phase: Equatable, Sendable {
        case awaitingSession
        case configuring
        case ready
        case starting
        case capturing
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

    private let audioService: any SidecarAudioService
    private let recognitionService: any SidecarRecognitionService
    private let playbackService: any SidecarPlaybackService
    private let eventSink: any SidecarEventSink

    private var configuration: SidecarConfiguration?
    private var phase = Phase.awaitingSession
    private var audioState = ServiceState.notAttempted
    private var recognitionState = ServiceState.notAttempted
    private var playbackStopped = false
    private var cleanupStarted = false
    private var playbackBuffer = PlaybackBuffer()
    private var bargeInGate: BargeInGate?

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
        try requireAvailableOperation()
        guard let audio = frame.audio else {
            try await fail(
                SidecarSessionError.expectedAudioFrame,
                fallbackSessionID: configuration?.sessionID ?? 0
            )
        }

        do {
            let configuration = try requireCapturing()
            guard audio.sessionID == configuration.sessionID else {
                throw SidecarSessionError.sessionMismatch
            }

            var nextBuffer = playbackBuffer
            try nextBuffer.enqueue(audio.frame)
            playbackBuffer = nextBuffer
            try await playbackService.enqueue(audio.frame)
            guard phase == .capturing,
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
        try requireAvailableOperation()
        do {
            let configuration = try requireCapturing()
            try playbackBuffer.markRendered(identity)
            try await eventSink.send(
                ChildFrame(
                    control: .playbackRendered(
                        sessionID: configuration.sessionID,
                        turnID: identity.turnID,
                        generationID: identity.generationID,
                        utteranceID: identity.utteranceID,
                        sequence: identity.sequence
                    )
                )
            )
        } catch {
            try await fail(error, fallbackSessionID: configuration?.sessionID ?? 0)
        }
    }

    public func publishVoiceActivity(_ activity: VoiceActivity) async throws {
        try requireAvailableOperation()
        do {
            let configuration = try requireCapturing()
            try await eventSink.send(
                ChildFrame(
                    control: .voiceActivity(
                        sessionID: configuration.sessionID,
                        activity: activity
                    )
                )
            )
        } catch {
            try await fail(error, fallbackSessionID: configuration?.sessionID ?? 0)
        }
    }

    public func publishRecognitionHypothesis(
        _ hypothesis: RecognitionHypothesis
    ) async throws {
        try requireAvailableOperation()
        do {
            let configuration = try requireCapturing()
            try await eventSink.send(
                ChildFrame(
                    control: .transcriptHypothesis(
                        sessionID: configuration.sessionID,
                        hypothesis: hypothesis
                    )
                )
            )
        } catch {
            try await fail(error, fallbackSessionID: configuration?.sessionID ?? 0)
        }
    }

    @discardableResult
    public func observeBargeIn(
        isSpeech: Bool,
        frameMilliseconds: UInt64,
        atMilliseconds: UInt64
    ) async throws -> Bool {
        try requireAvailableOperation()
        do {
            let configuration = try requireCapturing()
            guard playbackBuffer.isPlaybackActive,
                  let generationID = playbackBuffer.activeGenerationID
            else {
                bargeInGate?.reset()
                return false
            }
            guard bargeInGate?.observe(
                isSpeech: isSpeech,
                frameMilliseconds: frameMilliseconds
            ) == true else {
                return false
            }

            let resumePhase = try reserveFlush(
                throughGenerationID: generationID
            )
            try await playbackService.flush(
                throughGenerationID: generationID
            )
            bargeInGate?.reset()
            try await eventSink.send(
                ChildFrame(
                    control: .voiceActivity(
                        sessionID: configuration.sessionID,
                        activity: .speechStarted(atMilliseconds: atMilliseconds)
                    )
                )
            )
            restoreAfterFlush(resumePhase)
            return true
        } catch {
            try await fail(error, fallbackSessionID: configuration?.sessionID ?? 0)
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
            bargeInGate = BargeInGate(thresholdMilliseconds: speechStartMilliseconds)
            phase = .configuring
            try await eventSink.send(ChildFrame(control: .ready(sessionID: sessionID)))
            phase = .ready

        case let .startCapture(sessionID):
            let configuration = try requireReady(sessionID: sessionID)
            phase = .starting
            audioState = .attempted
            try await audioService.start(configuration: configuration)
            audioState = .started
            recognitionState = .attempted
            try await recognitionService.start(configuration: configuration)
            recognitionState = .started
            phase = .capturing

        case let .flushGeneration(sessionID, generationID, operationID):
            let configuration = try requireConfigured(sessionID: sessionID)
            let resumePhase = try reserveFlush(
                throughGenerationID: generationID
            )
            try await playbackService.flush(
                throughGenerationID: generationID
            )
            bargeInGate?.reset()
            try await eventSink.send(
                ChildFrame(
                    control: .playbackFlushed(
                        sessionID: configuration.sessionID,
                        generationID: generationID,
                        operationID: operationID
                    )
                )
            )
            restoreAfterFlush(resumePhase)

        case let .shutdown(sessionID):
            let configuration = try requireConfigured(sessionID: sessionID)
            guard phase == .ready || phase == .capturing else {
                throw SidecarSessionError.invalidState
            }
            phase = .terminating
            await cleanupServices()
            try await eventSink.send(
                ChildFrame(control: .shutdownComplete(sessionID: configuration.sessionID))
            )
            phase = .terminated

        case .ready,
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

    private func reserveFlush(
        throughGenerationID generationID: UInt64
    ) throws -> StablePhase {
        let resumePhase: StablePhase
        switch phase {
        case .ready:
            resumePhase = .ready
        case .capturing:
            resumePhase = .capturing
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

    private func requireAvailableOperation() throws {
        switch phase {
        case .awaitingSession, .ready, .capturing:
            return
        case .configuring,
             .starting,
             .flushing,
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

        phase = .failing
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

        if recognitionState == .attempted || recognitionState == .started {
            recognitionState = .stopped
            await recognitionService.stop()
        }
        if audioState == .attempted || audioState == .started {
            audioState = .stopped
            await audioService.stop()
        }
        if !playbackStopped {
            playbackStopped = true
            await playbackService.stop()
        }
    }
}
