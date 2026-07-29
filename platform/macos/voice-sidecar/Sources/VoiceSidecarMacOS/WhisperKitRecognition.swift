import Foundation
import VoiceSidecarCore
@preconcurrency import WhisperKit

public struct RecognitionSegmentSnapshot: Equatable, Sendable {
    public let id: Int
    public let text: String

    public init(id: Int, text: String) {
        self.id = id
        self.text = text
    }
}

public struct RecognitionStateSnapshot: Equatable, Sendable {
    public let currentText: String
    public let confirmedSegments: [RecognitionSegmentSnapshot]
    public let unconfirmedSegments: [RecognitionSegmentSnapshot]

    public init(
        currentText: String = "",
        confirmedSegments: [RecognitionSegmentSnapshot] = [],
        unconfirmedSegments: [RecognitionSegmentSnapshot] = []
    ) {
        self.currentText = currentText
        self.confirmedSegments = confirmedSegments
        self.unconfirmedSegments = unconfirmedSegments
    }
}

public struct RecognitionMapper: Sendable {
    public init() {}

    public func changes(
        from oldState: RecognitionStateSnapshot,
        to newState: RecognitionStateSnapshot
    ) -> [RecognitionHypothesis] {
        var changes: [RecognitionHypothesis] = []
        let confirmedCount = min(
            oldState.confirmedSegments.count,
            newState.confirmedSegments.count
        )
        for (offset, segment) in newState.confirmedSegments
            .dropFirst(confirmedCount)
            .enumerated()
        {
            guard
                !segment.text.trimmingCharacters(
                    in: .whitespacesAndNewlines
                ).isEmpty
            else {
                continue
            }
            changes.append(
                RecognitionHypothesis(
                    segmentID: UInt64(confirmedCount + offset),
                    text: segment.text,
                    engineFinal: true
                )
            )
        }

        let oldPartial = partialText(oldState)
        let newPartial = partialText(newState)
        if newPartial != oldPartial,
            let newPartial,
            let segmentID = partialSegmentID(newState)
        {
            changes.append(
                RecognitionHypothesis(
                    segmentID: segmentID,
                    text: newPartial,
                    engineFinal: false
                )
            )
        }
        return changes
    }

    private func partialText(
        _ state: RecognitionStateSnapshot
    ) -> String? {
        let currentText = state.currentText
            .trimmingCharacters(in: .whitespacesAndNewlines)
        if !currentText.isEmpty, currentText != "Waiting for speech..." {
            return state.currentText
        }
        let unconfirmed = state.unconfirmedSegments
            .map(\.text)
            .joined()
        guard
            !unconfirmed.trimmingCharacters(
                in: .whitespacesAndNewlines
            ).isEmpty
        else {
            return nil
        }
        return unconfirmed
    }

    private func partialSegmentID(
        _ state: RecognitionStateSnapshot
    ) -> UInt64? {
        UInt64(
            state.confirmedSegments.count
                + max(0, state.unconfirmedSegments.count - 1)
        )
    }
}

public enum WhisperKitRecognitionEvent: Sendable {
    case hypothesis(RecognitionHypothesis)
    case voiceWindow(
        isSpeech: Bool,
        frameMilliseconds: UInt64,
        atMilliseconds: UInt64
    )
    case activity(VoiceActivity)
    case failure(
        sessionID: UInt64,
        failure: SidecarServiceFailure
    )
}

private actor RecognitionEventRelay {
    typealias Handler =
        @Sendable (
            WhisperKitRecognitionEvent
        ) async throws -> Bool

    private var handler: Handler?

    func setHandler(_ handler: Handler?) {
        self.handler = handler
    }

    func emit(_ event: WhisperKitRecognitionEvent) async -> Bool {
        do {
            return try await handler?(event) ?? false
        } catch {
            return false
        }
    }
}

public actor WhisperKitRecognition: SidecarRecognitionService {
    public typealias EventHandler =
        @Sendable (
            WhisperKitRecognitionEvent
        ) async throws -> Bool

    private let modelPath: String
    private let audioProcessor: VoiceProcessingAudioProcessor
    private let mapper = RecognitionMapper()
    private let vad = EnergyVAD(
        sampleRate: 16_000,
        frameLength: 0.1
    )
    private let eventRelay = RecognitionEventRelay()

    private var transcriber: AudioStreamTranscriber?
    private var transcriptionTask: Task<Void, Never>?
    private var voiceWindowTask: Task<Void, Never>?
    private var voiceWindowContinuation:
        AsyncStream<
            [Float]
        >.Continuation?
    private var startTimeNanoseconds: UInt64 = 0
    private var activeSessionID: UInt64?
    private var speechStartMilliseconds: UInt64 = 200
    private var positiveSpeechMilliseconds: UInt64 = 0
    private var speaking = false
    private var stopping = false

    public init(
        modelPath: String,
        audioProcessor: VoiceProcessingAudioProcessor
    ) {
        self.modelPath = modelPath
        self.audioProcessor = audioProcessor
    }

    public func setEventHandler(_ handler: EventHandler?) async {
        await eventRelay.setHandler(handler)
    }

    public func start(configuration: SidecarConfiguration) async throws {
        guard transcriptionTask == nil else {
            throw SidecarServiceFailure(
                stage: .speechRecognizer,
                code: .invalidState
            )
        }
        let modelURL = URL(fileURLWithPath: modelPath, isDirectory: true)
        var modelIsDirectory = ObjCBool(false)
        let tokenizerURL = modelURL.appendingPathComponent(
            "tokenizer.json",
            isDirectory: false
        )
        guard (modelPath as NSString).isAbsolutePath,
            FileManager.default.fileExists(
                atPath: modelURL.path,
                isDirectory: &modelIsDirectory
            ),
            modelIsDirectory.boolValue,
            FileManager.default.fileExists(atPath: tokenizerURL.path)
        else {
            throw SidecarServiceFailure(
                stage: .speechRecognizer,
                code: .recognitionFailed
            )
        }

        stopping = false
        activeSessionID = configuration.sessionID
        speechStartMilliseconds = configuration.speechStartMilliseconds
        positiveSpeechMilliseconds = 0
        speaking = false
        startTimeNanoseconds = DispatchTime.now().uptimeNanoseconds

        let whisperKit: WhisperKit
        do {
            whisperKit = try await WhisperKit(
                modelFolder: modelURL.path,
                tokenizerFolder: modelURL,
                audioProcessor: audioProcessor,
                verbose: false,
                load: true,
                download: false
            )
        } catch {
            throw SidecarServiceFailure(
                stage: .speechRecognizer,
                code: .recognitionFailed
            )
        }
        guard let tokenizer = whisperKit.tokenizer else {
            throw SidecarServiceFailure(
                stage: .speechRecognizer,
                code: .recognitionFailed
            )
        }

        let mapper = mapper
        let eventRelay = eventRelay
        let transcriber = AudioStreamTranscriber(
            audioEncoder: whisperKit.audioEncoder,
            featureExtractor: whisperKit.featureExtractor,
            segmentSeeker: whisperKit.segmentSeeker,
            textDecoder: whisperKit.textDecoder,
            tokenizer: tokenizer,
            audioProcessor: audioProcessor,
            decodingOptions: DecodingOptions(),
            requiredSegmentsForConfirmation: 0,
            stateChangeCallback: { oldState, newState in
                let hypotheses = mapper.changes(
                    from: Self.snapshot(oldState),
                    to: Self.snapshot(newState)
                )
                guard !hypotheses.isEmpty else {
                    return
                }
                Task {
                    for hypothesis in hypotheses {
                        _ = await eventRelay.emit(
                            .hypothesis(hypothesis)
                        )
                    }
                }
            }
        )
        self.transcriber = transcriber

        let (voiceWindows, continuation) = AsyncStream<
            [Float]
        >.makeStream(bufferingPolicy: .bufferingNewest(8))
        voiceWindowContinuation = continuation
        audioProcessor.setVoiceWindowHandler { window in
            continuation.yield(window)
        }
        voiceWindowTask = Task { [weak self] in
            for await window in voiceWindows {
                guard !Task.isCancelled else {
                    return
                }
                await self?.processVoiceWindow(window)
            }
        }

        transcriptionTask = Task { [weak self] in
            do {
                try await transcriber.startStreamTranscription()
                await self?.recognitionStoppedUnexpectedly()
            } catch {
                await self?.recognitionStoppedUnexpectedly()
            }
        }
        await audioProcessor.waitUntilRecordingStarted()
    }

    public func stop() async {
        stopping = true
        audioProcessor.setVoiceWindowHandler(nil)
        voiceWindowContinuation?.finish()
        voiceWindowContinuation = nil
        voiceWindowTask?.cancel()
        if let voiceWindowTask {
            await voiceWindowTask.value
        }
        self.voiceWindowTask = nil

        if let transcriber {
            await transcriber.stopStreamTranscription()
        }
        transcriptionTask?.cancel()
        if let transcriptionTask {
            await transcriptionTask.value
        }
        self.transcriptionTask = nil
        self.transcriber = nil
        activeSessionID = nil
        audioProcessor.stopRecording()

        await eventRelay.setHandler(nil)
    }

    private static func snapshot(
        _ state: AudioStreamTranscriber.State
    ) -> RecognitionStateSnapshot {
        RecognitionStateSnapshot(
            currentText: state.currentText,
            confirmedSegments: state.confirmedSegments.map {
                RecognitionSegmentSnapshot(id: $0.id, text: $0.text)
            },
            unconfirmedSegments: state.unconfirmedSegments.map {
                RecognitionSegmentSnapshot(id: $0.id, text: $0.text)
            }
        )
    }

    private func processVoiceWindow(_ window: [Float]) async {
        let isSpeech = vad.voiceActivity(in: window).last == true
        let atMilliseconds =
            (DispatchTime.now().uptimeNanoseconds
                &- startTimeNanoseconds) / 1_000_000
        let didBargeIn = await eventRelay.emit(
            .voiceWindow(
                isSpeech: isSpeech,
                frameMilliseconds: 100,
                atMilliseconds: atMilliseconds
            )
        )

        if isSpeech {
            positiveSpeechMilliseconds = min(
                speechStartMilliseconds,
                positiveSpeechMilliseconds + 100
            )
            if !speaking,
                positiveSpeechMilliseconds >= speechStartMilliseconds
            {
                speaking = true
                if !didBargeIn {
                    _ = await eventRelay.emit(
                        .activity(
                            .speechStarted(
                                atMilliseconds: atMilliseconds
                            )
                        )
                    )
                }
            } else if speaking {
                _ = await eventRelay.emit(
                    .activity(
                        .speechContinued(
                            atMilliseconds: atMilliseconds
                        )
                    )
                )
            }
        } else {
            positiveSpeechMilliseconds = 0
            if speaking {
                speaking = false
                _ = await eventRelay.emit(
                    .activity(
                        .speechEnded(
                            atMilliseconds: atMilliseconds
                        )
                    )
                )
            }
        }
    }

    private func recognitionStoppedUnexpectedly() async {
        guard !stopping else {
            return
        }
        guard let activeSessionID else {
            return
        }
        _ = await eventRelay.emit(
            .failure(
                sessionID: activeSessionID,
                failure: SidecarServiceFailure(
                    stage: .speechRecognizer,
                    code: .recognitionFailed
                )
            )
        )
    }
}
