import Foundation
import NaturalLanguage
import VoiceSidecarCore
@preconcurrency import WhisperKit

public final class OfflineWhisperTokenizer:
    WhisperTokenizer,
    @unchecked Sendable
{
    private let tokenizer: TokenizerWrapper
    public let specialTokens: SpecialTokens
    public let allLanguageTokens: Set<Int>

    private init(tokenizer: TokenizerWrapper) {
        self.tokenizer = tokenizer
        let specialTokens = SpecialTokens(
            endToken: tokenizer.convertTokenToId("<|endoftext|>") ?? 50_257,
            englishToken: tokenizer.convertTokenToId("<|en|>") ?? 50_259,
            noSpeechToken: tokenizer.convertTokenToId("<|nospeech|>") ?? 50_362,
            noTimestampsToken:
                tokenizer.convertTokenToId("<|notimestamps|>") ?? 50_363,
            specialTokenBegin:
                tokenizer.convertTokenToId("<|endoftext|>") ?? 50_257,
            startOfPreviousToken:
                tokenizer.convertTokenToId("<|startofprev|>") ?? 50_361,
            startOfTranscriptToken:
                tokenizer.convertTokenToId("<|startoftranscript|>") ?? 50_258,
            timeTokenBegin:
                tokenizer.convertTokenToId("<|0.00|>") ?? 50_364,
            transcribeToken:
                tokenizer.convertTokenToId("<|transcribe|>") ?? 50_359,
            translateToken:
                tokenizer.convertTokenToId("<|translate|>") ?? 50_358,
            whitespaceToken: tokenizer.convertTokenToId(" ") ?? 220
        )
        self.specialTokens = specialTokens
        allLanguageTokens = Set(
            Constants.languages.values
                .compactMap {
                    tokenizer.convertTokenToId("<|\($0)|>")
                }
                .filter { $0 > specialTokens.specialTokenBegin }
        )
    }

    public static func load(
        from modelFolder: URL
    ) async throws -> OfflineWhisperTokenizer {
        let tokenizer = try await AutoTokenizerWrapper.from(
            modelFolder: modelFolder,
            strict: true
        )
        return OfflineWhisperTokenizer(tokenizer: tokenizer)
    }

    public func encode(text: String) -> [Int] {
        tokenizer.encode(text: text)
    }

    public func decode(tokens: [Int]) -> String {
        tokenizer.decode(tokens: tokens)
    }

    public func convertTokenToId(_ token: String) -> Int? {
        tokenizer.convertTokenToId(token)
    }

    public func convertIdToToken(_ id: Int) -> String? {
        tokenizer.convertIdToToken(id)
    }

    public func splitToWordTokens(
        tokenIds: [Int]
    ) -> (words: [String], wordTokens: [[Int]]) {
        let decodedWords = tokenizer.decode(
            tokens: tokenIds.filter {
                $0 < specialTokens.specialTokenBegin
            }
        )
        let recognizer = NLLanguageRecognizer()
        recognizer.processString(decodedWords)
        if ["zh", "ja", "th", "lo", "my", "yue"].contains(
            recognizer.dominantLanguage?.rawValue
        ) {
            return splitTokensOnUnicode(tokens: tokenIds)
        }
        return splitTokensOnSpaces(tokens: tokenIds)
    }

    private func splitTokensOnUnicode(
        tokens: [Int]
    ) -> (words: [String], wordTokens: [[Int]]) {
        let decodedFull = tokenizer.decode(tokens: tokens)
        let replacementString = "\u{fffd}"
        var words: [String] = []
        var wordTokens: [[Int]] = []
        var currentTokens: [Int] = []

        for token in tokens {
            currentTokens.append(token)
            let decoded = tokenizer.decode(tokens: currentTokens)
            var replacementExistsInFullText = false
            if let range = decoded.range(of: replacementString) {
                replacementExistsInFullText =
                    decodedFull[range] == replacementString
            }
            if !decoded.contains(replacementString)
                || replacementExistsInFullText
            {
                words.append(decoded)
                wordTokens.append(currentTokens)
                currentTokens = []
            }
        }
        return (words, wordTokens)
    }

    private func splitTokensOnSpaces(
        tokens: [Int]
    ) -> (words: [String], wordTokens: [[Int]]) {
        let (subwords, subwordTokensList) =
            splitTokensOnUnicode(tokens: tokens)
        var words: [String] = []
        var wordTokens: [[Int]] = []

        for (subword, subwordTokens) in zip(
            subwords,
            subwordTokensList
        ) {
            let special =
                subwordTokens.first! >= specialTokens.specialTokenBegin
            let withSpace = subword.hasPrefix(" ")
            let stripped = subword.trimmingCharacters(in: .whitespaces)
            let punctuation =
                UnicodeScalar(stripped).map {
                    CharacterSet.punctuationCharacters.contains($0)
                } ?? false
            if special || withSpace || punctuation || words.isEmpty {
                words.append(subword)
                wordTokens.append(subwordTokens)
            } else {
                words[words.count - 1] += subword
                wordTokens[words.count - 1].append(
                    contentsOf: subwordTokens
                )
            }
        }
        return (words, wordTokens)
    }
}

public final class OrderedRecognitionBatchPipeline: @unchecked Sendable {
    public typealias Handler =
        @Sendable ([RecognitionHypothesis]) async -> Void
    public typealias FailureHandler = @Sendable () async -> Void

    private let stream: AsyncStream<[RecognitionHypothesis]>
    private let continuation:
        AsyncStream<
            [RecognitionHypothesis]
        >.Continuation
    private let stateLock = NSLock()
    private var consumer: Task<Void, Never>?
    private var failureHandler: FailureHandler?
    private var failed = false

    public init(capacity: Int) {
        (stream, continuation) = AsyncStream.makeStream(
            bufferingPolicy: .bufferingOldest(max(1, capacity))
        )
    }

    public func start(
        _ handler: @escaping Handler,
        failureHandler: FailureHandler? = nil
    ) {
        stateLock.withLock {
            guard consumer == nil else {
                return
            }
            self.failureHandler = failureHandler
            let stream = stream
            consumer = Task { [weak self] in
                for await batch in stream {
                    await handler(batch)
                }
                await self?.reportOverflowIfNeeded()
            }
        }
    }

    @discardableResult
    public func enqueue(
        _ batch: [RecognitionHypothesis]
    ) -> Bool {
        guard !batch.isEmpty else {
            return true
        }
        switch continuation.yield(batch) {
        case .enqueued:
            return true
        case .dropped:
            fail()
            return false
        case .terminated:
            return false
        @unknown default:
            fail()
            return false
        }
    }

    public func fail() {
        stateLock.withLock {
            failed = true
        }
        continuation.finish()
    }

    public func finish() async {
        continuation.finish()
        let task = stateLock.withLock {
            consumer
        }
        await task?.value
        stateLock.withLock {
            consumer = nil
            failureHandler = nil
        }
    }

    public func cancel() {
        continuation.finish()
        let task = stateLock.withLock {
            let task = consumer
            consumer = nil
            failureHandler = nil
            return task
        }
        task?.cancel()
    }

    private func reportOverflowIfNeeded() async {
        let handler = stateLock.withLock {
            failed ? failureHandler : nil
        }
        await handler?()
    }
}

public actor SidecarFailureController {
    private let session: SidecarSession
    private let exitHandler: @Sendable () -> Void
    private var terminating = false

    public init(
        session: SidecarSession,
        exitHandler: @escaping @Sendable () -> Void
    ) {
        self.session = session
        self.exitHandler = exitHandler
    }

    public func perform(
        fallbackSessionID: UInt64 = 0,
        _ operation: @Sendable () async throws -> Void
    ) async {
        guard !terminating else {
            return
        }
        do {
            try await operation()
        } catch {
            await terminate(
                with: error,
                fallbackSessionID: fallbackSessionID
            )
        }
    }

    public func performBool(
        fallbackSessionID: UInt64 = 0,
        _ operation: @Sendable () async throws -> Bool
    ) async -> Bool {
        guard !terminating else {
            return false
        }
        do {
            return try await operation()
        } catch {
            await terminate(
                with: error,
                fallbackSessionID: fallbackSessionID
            )
            return false
        }
    }

    public func terminate(
        with error: any Error,
        fallbackSessionID: UInt64
    ) async {
        guard !terminating else {
            return
        }
        terminating = true
        do {
            try await session.terminateFromServiceFailure(
                error,
                fallbackSessionID: fallbackSessionID
            )
        } catch {}
        exitHandler()
    }
}

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
        ) async -> Bool

    private var handler: Handler?

    func setHandler(_ handler: Handler?) {
        self.handler = handler
    }

    func emit(_ event: WhisperKitRecognitionEvent) async -> Bool {
        await handler?(event) ?? false
    }
}

public actor WhisperKitRecognition: SidecarRecognitionService {
    public typealias EventHandler =
        @Sendable (
            WhisperKitRecognitionEvent
        ) async -> Bool

    private let modelPath: String
    private let audioProcessor: VoiceProcessingAudioProcessor
    private let mapper = RecognitionMapper()
    private let vad = EnergyVAD(
        sampleRate: 16_000,
        frameLength: 0.1
    )
    private let eventRelay = RecognitionEventRelay()

    private var transcriber: AudioStreamTranscriber?
    private var hypothesisPipeline: OrderedRecognitionBatchPipeline?
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

        let tokenizer: OfflineWhisperTokenizer
        let whisperKit: WhisperKit
        do {
            tokenizer = try await OfflineWhisperTokenizer.load(
                from: modelURL
            )
            whisperKit = try await WhisperKit(
                modelFolder: modelURL.path,
                audioProcessor: audioProcessor,
                verbose: false,
                load: false,
                download: false
            )
            whisperKit.tokenizer = tokenizer
            try await whisperKit.loadModels()
        } catch {
            throw SidecarServiceFailure(
                stage: .speechRecognizer,
                code: .recognitionFailed
            )
        }

        let mapper = mapper
        let eventRelay = eventRelay
        let pipeline = OrderedRecognitionBatchPipeline(capacity: 8)
        pipeline.start(
            { hypotheses in
                for hypothesis in hypotheses {
                    _ = await eventRelay.emit(
                        .hypothesis(hypothesis)
                    )
                }
            },
            failureHandler: { [weak self] in
                await self?.recognitionStoppedUnexpectedly()
            }
        )
        hypothesisPipeline = pipeline
        audioProcessor.setConversionFailureHandler { _ in
            pipeline.fail()
        }
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
                pipeline.enqueue(hypotheses)
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

        hypothesisPipeline?.cancel()
        hypothesisPipeline = nil
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
