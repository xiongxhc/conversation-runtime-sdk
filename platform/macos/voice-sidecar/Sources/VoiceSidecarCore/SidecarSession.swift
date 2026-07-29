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

    private let audioService: any SidecarAudioService
    private let recognitionService: any SidecarRecognitionService
    private let playbackService: any SidecarPlaybackService
    private let eventSink: any SidecarEventSink

    private var configuration: SidecarConfiguration?
    private var captureStarted = false
    private var failed = false
    public private(set) var isTerminated = false
    private var playbackBuffer = PlaybackBuffer()
    private var bargeInGate: BargeInGate?

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
            try await playbackService.enqueue(audio.frame)
            playbackBuffer = nextBuffer
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

            try await flushLocally(throughGenerationID: generationID)
            try await eventSink.send(
                ChildFrame(
                    control: .voiceActivity(
                        sessionID: configuration.sessionID,
                        activity: .speechStarted(atMilliseconds: atMilliseconds)
                    )
                )
            )
            return true
        } catch {
            try await fail(error, fallbackSessionID: configuration?.sessionID ?? 0)
        }
    }

    private func process(_ control: ChildControl) async throws {
        guard !failed, !isTerminated else {
            throw SidecarSessionError.invalidState
        }

        switch control {
        case let .startSession(
            sessionID,
            speechStartMilliseconds,
            finalSilenceMilliseconds
        ):
            guard configuration == nil else {
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
            try await eventSink.send(ChildFrame(control: .ready(sessionID: sessionID)))

        case let .startCapture(sessionID):
            let configuration = try requireReady(sessionID: sessionID)
            try await audioService.start(configuration: configuration)
            do {
                try await recognitionService.start(configuration: configuration)
            } catch {
                await audioService.stop()
                throw error
            }
            captureStarted = true

        case let .flushGeneration(sessionID, generationID, operationID):
            let configuration = try requireConfigured(sessionID: sessionID)
            try await flushLocally(throughGenerationID: generationID)
            try await eventSink.send(
                ChildFrame(
                    control: .playbackFlushed(
                        sessionID: configuration.sessionID,
                        generationID: generationID,
                        operationID: operationID
                    )
                )
            )

        case let .shutdown(sessionID):
            let configuration = try requireConfigured(sessionID: sessionID)
            await recognitionService.stop()
            await audioService.stop()
            await playbackService.stop()
            captureStarted = false
            isTerminated = true
            try await eventSink.send(
                ChildFrame(control: .shutdownComplete(sessionID: configuration.sessionID))
            )

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

    private func flushLocally(throughGenerationID generationID: UInt64) async throws {
        var nextBuffer = playbackBuffer
        _ = try nextBuffer.flush(throughGenerationID: generationID)
        try await playbackService.flush(throughGenerationID: generationID)
        playbackBuffer = nextBuffer
        bargeInGate?.reset()
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
        guard !captureStarted else {
            throw SidecarSessionError.invalidState
        }
        return configuration
    }

    private func requireCapturing() throws -> SidecarConfiguration {
        guard captureStarted, let configuration, !failed, !isTerminated else {
            throw SidecarSessionError.invalidState
        }
        return configuration
    }

    private func fail(
        _ error: any Error,
        fallbackSessionID: UInt64
    ) async throws -> Never {
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

        failed = true
        if captureStarted {
            await recognitionService.stop()
            await audioService.stop()
            captureStarted = false
        }
        await playbackService.stop()
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
}
