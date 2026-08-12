import Foundation
import NaturalLanguage
import VoiceSidecarCore
@preconcurrency import WhisperKit

struct UnicodeTokenSplitter {
    static func split(
        tokens: [Int],
        decode: ([Int]) -> String
    ) -> (words: [String], wordTokens: [[Int]]) {
        let decodedFull = decode(tokens)
        let replacementString = "\u{fffd}"
        var words: [String] = []
        var wordTokens: [[Int]] = []
        var currentTokens: [Int] = []

        for token in tokens {
            currentTokens.append(token)
            let decoded = decode(currentTokens)
            let replacementExistsInFullText =
                decoded.range(of: replacementString).map {
                    replacementExists(
                        decodedRange: $0,
                        decoded: decoded,
                        decodedFull: decodedFull,
                        replacementString: replacementString
                    )
                } ?? false
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

    private static func replacementExists(
        decodedRange: Range<String.Index>,
        decoded: String,
        decodedFull: String,
        replacementString: String
    ) -> Bool {
        guard
            let decodedLower = decodedRange.lowerBound.samePosition(
                in: decoded.utf8
            ),
            let decodedUpper = decodedRange.upperBound.samePosition(
                in: decoded.utf8
            )
        else {
            return false
        }
        let lowerOffset = decoded.utf8.distance(
            from: decoded.utf8.startIndex,
            to: decodedLower
        )
        let upperOffset = decoded.utf8.distance(
            from: decoded.utf8.startIndex,
            to: decodedUpper
        )
        guard
            let fullLower = decodedFull.utf8.index(
                decodedFull.utf8.startIndex,
                offsetBy: lowerOffset,
                limitedBy: decodedFull.utf8.endIndex
            ),
            let fullUpper = decodedFull.utf8.index(
                decodedFull.utf8.startIndex,
                offsetBy: upperOffset,
                limitedBy: decodedFull.utf8.endIndex
            )
        else {
            return false
        }
        return decodedFull.utf8[fullLower..<fullUpper].elementsEqual(
            replacementString.utf8
        )
    }
}

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
        UnicodeTokenSplitter.split(tokens: tokens) {
            tokenizer.decode(tokens: $0)
        }
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

public enum RecognitionWorkerCompletion: Equatable, Sendable {
    case stopped
    case failed(SidecarServiceFailure)
}

public enum RecognitionBatchEnqueueResult: Equatable, Sendable {
    case queued
    case coalescedPartial
    case evictedPartial
    case droppedPartial
    case finalOverflow
    case terminated
}

private final class RecognitionBatchMailbox: @unchecked Sendable {
    private let capacity: Int
    private let lock = NSLock()
    private let notifications: AsyncStream<Void>
    private let continuation: AsyncStream<Void>.Continuation
    private var batches: [[RecognitionHypothesis]] = []
    private var terminal: RecognitionWorkerCompletion?
    private var maximumCount = 0

    var stream: AsyncStream<Void> {
        notifications
    }

    var pendingCount: Int {
        lock.withLock { batches.count }
    }

    var maximumPendingCount: Int {
        lock.withLock { maximumCount }
    }

    init(capacity: Int) {
        self.capacity = max(1, capacity)
        (notifications, continuation) = AsyncStream.makeStream(
            bufferingPolicy: .bufferingNewest(1)
        )
    }

    func enqueue(
        _ batch: [RecognitionHypothesis]
    ) -> RecognitionBatchEnqueueResult {
        guard !batch.isEmpty else {
            return .queued
        }
        let result = lock.withLock {
            guard terminal == nil else {
                return RecognitionBatchEnqueueResult.terminated
            }
            let containsFinal = batch.contains(where: \.engineFinal)
            if !containsFinal,
                batches.last?.allSatisfy({
                    !$0.engineFinal
                }) == true
            {
                batches[batches.count - 1] = batch
                return .coalescedPartial
            }
            if batches.count < capacity {
                batches.append(batch)
                maximumCount = max(maximumCount, batches.count)
                return .queued
            }
            if !containsFinal {
                return .droppedPartial
            }
            if let partialIndex = batches.firstIndex(where: {
                $0.allSatisfy { !$0.engineFinal }
            }) {
                batches.remove(at: partialIndex)
                batches.append(batch)
                return .evictedPartial
            }
            terminal = .failed(
                SidecarServiceFailure(
                    stage: .speechRecognizer,
                    code: .recognitionFailed
                )
            )
            return .finalOverflow
        }
        continuation.yield()
        if result == .finalOverflow {
            continuation.finish()
        }
        return result
    }

    func dequeue() -> [RecognitionHypothesis]? {
        lock.withLock {
            guard !batches.isEmpty else {
                return nil
            }
            return batches.removeFirst()
        }
    }

    func completionIfDrained() -> RecognitionWorkerCompletion? {
        lock.withLock {
            batches.isEmpty ? terminal : nil
        }
    }

    func finish(
        with completion: RecognitionWorkerCompletion = .stopped
    ) {
        lock.withLock {
            if terminal == nil {
                terminal = completion
            }
        }
        continuation.finish()
    }

    func cancel() {
        lock.withLock {
            batches.removeAll(keepingCapacity: true)
            terminal = .stopped
        }
        continuation.finish()
    }
}

public final class OrderedRecognitionBatchPipeline: @unchecked Sendable {
    public typealias Handler =
        @Sendable ([RecognitionHypothesis]) async throws -> Void

    private let mailbox: RecognitionBatchMailbox
    private let stateLock = NSLock()
    private var consumer: Task<RecognitionWorkerCompletion, Never>?

    public var pendingCount: Int {
        mailbox.pendingCount
    }

    public var maximumPendingCount: Int {
        mailbox.maximumPendingCount
    }

    public init(capacity: Int) {
        mailbox = RecognitionBatchMailbox(capacity: capacity)
    }

    @discardableResult
    public func start(
        _ handler: @escaping Handler
    ) -> Task<RecognitionWorkerCompletion, Never> {
        stateLock.withLock {
            if let consumer {
                return consumer
            }
            let mailbox = mailbox
            let task = Task<RecognitionWorkerCompletion, Never> {
                for await _ in mailbox.stream {
                    while let batch = mailbox.dequeue() {
                        guard !Task.isCancelled else {
                            return .stopped
                        }
                        do {
                            try await handler(batch)
                        } catch {
                            let failure = SidecarServiceFailure(
                                stage: .speechRecognizer,
                                code: .recognitionFailed
                            )
                            mailbox.finish(with: .failed(failure))
                            return .failed(failure)
                        }
                    }
                    if let completion = mailbox.completionIfDrained() {
                        return completion
                    }
                }
                while let batch = mailbox.dequeue() {
                    do {
                        try await handler(batch)
                    } catch {
                        let failure = SidecarServiceFailure(
                            stage: .speechRecognizer,
                            code: .recognitionFailed
                        )
                        mailbox.finish(with: .failed(failure))
                        return .failed(failure)
                    }
                }
                return mailbox.completionIfDrained() ?? .stopped
            }
            consumer = task
            return task
        }
    }

    @discardableResult
    public func enqueue(
        _ batch: [RecognitionHypothesis]
    ) -> RecognitionBatchEnqueueResult {
        mailbox.enqueue(batch)
    }

    public func fail(_ failure: SidecarServiceFailure) {
        mailbox.finish(with: .failed(failure))
    }

    public func finish() async -> RecognitionWorkerCompletion {
        mailbox.finish()
        let task = stateLock.withLock { consumer }
        let completion = await task?.value ?? .stopped
        stateLock.withLock {
            consumer = nil
        }
        return completion
    }

    public func cancel() async {
        mailbox.cancel()
        let task = stateLock.withLock {
            let task = consumer
            consumer = nil
            return task
        }
        task?.cancel()
        _ = await task?.value
    }
}

final class RecognitionWorkerStopState: @unchecked Sendable {
    private enum State {
        case running
        case restarting
        case stopping
    }

    private let lock = NSLock()
    private var state = State.running

    var isStopping: Bool {
        lock.withLock { state == .stopping }
    }

    var isWorkerExitExpected: Bool {
        lock.withLock { state != .running }
    }

    func beginRestart() -> Bool {
        lock.withLock {
            guard state == .running else {
                return false
            }
            state = .restarting
            return true
        }
    }

    func finishRestart() {
        lock.withLock {
            if state == .restarting {
                state = .running
            }
        }
    }

    func beginStopping() {
        lock.withLock {
            state = .stopping
        }
    }
}

func runRecognitionWorker(
    stopState: RecognitionWorkerStopState,
    operation: @escaping @Sendable () async throws -> Void
) async -> RecognitionWorkerCompletion {
    do {
        try await operation()
    } catch {
        if stopState.isWorkerExitExpected || Task.isCancelled {
            return .stopped
        }
        return .failed(
            SidecarServiceFailure(
                stage: .speechRecognizer,
                code: .recognitionFailed
            )
        )
    }
    if stopState.isWorkerExitExpected || Task.isCancelled {
        return .stopped
    }
    return .failed(
        SidecarServiceFailure(
            stage: .speechRecognizer,
            code: .recognitionFailed
        )
    )
}

@discardableResult
func monitorRecognitionWorker(
    _ worker: Task<RecognitionWorkerCompletion, Never>,
    failureHandler: @escaping @Sendable (SidecarServiceFailure) async -> Void
) -> Task<Void, Never> {
    Task {
        let completion = await worker.value
        guard case .failed(let failure) = completion else {
            return
        }
        await failureHandler(failure)
    }
}

public actor SidecarFailureController {
    private let session: SidecarSession
    private let exitHandler: @Sendable () -> Void
    private var terminating = false
    private var recoveringRecognition = false

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

    public func reportRecoverableRecognitionFailure(
        _ failure: SidecarServiceFailure,
        fallbackSessionID: UInt64
    ) async {
        guard !terminating, !recoveringRecognition else {
            return
        }
        recoveringRecognition = true
        defer {
            recoveringRecognition = false
        }
        do {
            try await session.recoverFromRecognitionFailure(
                failure,
                fallbackSessionID: fallbackSessionID
            )
        } catch {
            await terminate(
                with: error,
                fallbackSessionID: fallbackSessionID
            )
        }
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
                ).isEmpty,
                containsRecognizableContent(segment.text)
            else {
                continue
            }
            changes.append(
                RecognitionHypothesis(
                    segmentID: UInt64(confirmedCount + offset + 1),
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
        if !currentText.isEmpty,
            currentText != "Waiting for speech...",
            containsRecognizableContent(currentText)
        {
            return state.currentText
        }
        let unconfirmed = state.unconfirmedSegments
            .map(\.text)
            .joined()
        guard
            !unconfirmed.trimmingCharacters(
                in: .whitespacesAndNewlines
            ).isEmpty,
            containsRecognizableContent(unconfirmed)
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
                + 1
        )
    }
}

private func containsRecognizableContent(_ text: String) -> Bool {
    text.unicodeScalars.contains { CharacterSet.alphanumerics.contains($0) }
}

public enum WhisperKitRecognitionEvent: Sendable {
    case hypothesis(RecognitionHypothesis)
    case voiceWindow(
        isSpeech: Bool,
        frameMilliseconds: UInt64,
        atMilliseconds: UInt64
    )
    case activity(VoiceActivity)
    case captureDiscontinuity(atMilliseconds: UInt64)
    case failure(
        sessionID: UInt64,
        failure: SidecarServiceFailure
    )
}

enum RecognitionSpeechTransition: Equatable, Sendable {
    case none
    case started
    case continued
    case ended
}

struct RecognitionSpeechGate: Sendable {
    private let thresholdMilliseconds: UInt64
    private var positiveMilliseconds: UInt64 = 0
    private var speaking = false

    var isSpeaking: Bool {
        speaking
    }

    init(thresholdMilliseconds: UInt64) {
        self.thresholdMilliseconds = thresholdMilliseconds
    }

    mutating func observe(
        isSpeech: Bool
    ) -> RecognitionSpeechTransition {
        if isSpeech {
            positiveMilliseconds = min(
                thresholdMilliseconds,
                positiveMilliseconds + 100
            )
            if !speaking,
                positiveMilliseconds >= thresholdMilliseconds
            {
                speaking = true
                return .started
            }
            return speaking ? .continued : .none
        }
        positiveMilliseconds = 0
        if speaking {
            speaking = false
            return .ended
        }
        return .none
    }

    mutating func resetForDiscontinuity() {
        positiveMilliseconds = 0
        speaking = false
    }
}

public struct SidecarRecognitionEventPublisher: Sendable {
    private let session: SidecarSession

    public init(session: SidecarSession) {
        self.session = session
    }

    @discardableResult
    public func publish(
        _ event: WhisperKitRecognitionEvent
    ) async throws -> Bool {
        switch event {
        case .hypothesis(let hypothesis):
            try await session.publishRecognitionHypothesisFromWorker(
                hypothesis
            )
            return false
        case .voiceWindow(
            let isSpeech,
            let frameMilliseconds,
            let atMilliseconds
        ):
            return try await session.observeBargeInFromRecognitionWorker(
                isSpeech: isSpeech,
                frameMilliseconds: frameMilliseconds,
                atMilliseconds: atMilliseconds
            )
        case .activity(let activity):
            try await session.publishVoiceActivityFromRecognitionWorker(
                activity
            )
            return false
        case .captureDiscontinuity(let atMilliseconds):
            try await session
                .observeCaptureDiscontinuityFromRecognitionWorker(
                    atMilliseconds: atMilliseconds
                )
            return false
        case .failure(_, let failure):
            throw failure
        }
    }
}

actor RecognitionEventRelay {
    typealias Handler =
        @Sendable (
            WhisperKitRecognitionEvent
        ) async throws -> Bool
    typealias FailureHandler =
        @Sendable (
            UInt64,
            SidecarServiceFailure
        ) async -> Void

    private var handler: Handler?
    private var failureHandler: FailureHandler?

    func setHandler(_ handler: Handler?) {
        self.handler = handler
    }

    func setFailureHandler(_ handler: FailureHandler?) {
        failureHandler = handler
    }

    func emit(_ event: WhisperKitRecognitionEvent) async throws -> Bool {
        guard let handler else {
            return false
        }
        return try await handler(event)
    }

    func reportFailure(
        sessionID: UInt64,
        failure: SidecarServiceFailure
    ) async {
        await failureHandler?(sessionID, failure)
    }
}

private enum RecognitionVoiceInput: Sendable {
    case window([Float])
    case discontinuity
}

private final class RecognitionVoiceMailbox: @unchecked Sendable {
    private let capacity: Int
    private let lock = NSLock()
    private let notifications: AsyncStream<Void>
    private let continuation: AsyncStream<Void>.Continuation
    private var inputs: [RecognitionVoiceInput] = []
    private var finished = false

    var stream: AsyncStream<Void> {
        notifications
    }

    init(capacity: Int) {
        self.capacity = max(2, capacity)
        (notifications, continuation) = AsyncStream.makeStream(
            bufferingPolicy: .bufferingNewest(1)
        )
    }

    func enqueue(_ input: RecognitionVoiceInput) {
        let shouldNotify = lock.withLock {
            guard !finished else {
                return false
            }
            switch input {
            case .discontinuity:
                inputs.removeAll(keepingCapacity: true)
                inputs.append(.discontinuity)
            case .window:
                if inputs.count >= capacity {
                    inputs.removeAll(keepingCapacity: true)
                    inputs.append(.discontinuity)
                }
                inputs.append(input)
            }
            return true
        }
        if shouldNotify {
            continuation.yield()
        }
    }

    func dequeue() -> RecognitionVoiceInput? {
        lock.withLock {
            guard !inputs.isEmpty else {
                return nil
            }
            return inputs.removeFirst()
        }
    }

    func finish() {
        lock.withLock {
            finished = true
            inputs.removeAll(keepingCapacity: true)
        }
        continuation.finish()
    }
}

public actor WhisperKitRecognition: SidecarRecognitionService {
    public typealias EventHandler =
        @Sendable (
            WhisperKitRecognitionEvent
        ) async throws -> Bool
    public typealias FailureHandler =
        @Sendable (
            UInt64,
            SidecarServiceFailure
        ) async -> Void

    private let modelPath: String
    private let audioProcessor: VoiceProcessingAudioProcessor
    private let turnAudioProcessor: TurnAudioProcessor
    private let language: String?
    private let mapper = RecognitionMapper()
    private let vad = EnergyVAD(
        sampleRate: 16_000,
        frameLength: 0.1
    )
    private let eventRelay = RecognitionEventRelay()

    private struct TranscriberComponents: @unchecked Sendable {
        let audioEncoder: any AudioEncoding
        let featureExtractor: any FeatureExtracting
        let segmentSeeker: any SegmentSeeking
        let textDecoder: any TextDecoding
        let tokenizer: any WhisperTokenizer
    }

    private var preparedConfiguration: SidecarConfiguration?
    private var transcriberComponents: TranscriberComponents?
    private var transcriber: AudioStreamTranscriber?
    private var hypothesisPipeline: OrderedRecognitionBatchPipeline?
    private var transcriptionTask: Task<RecognitionWorkerCompletion, Never>?
    private var transcriptionMonitorTask: Task<Void, Never>?
    private var recognitionResetTask: Task<Void, Never>?
    private var voiceWindowTask: Task<RecognitionWorkerCompletion, Never>?
    private var voiceMailbox: RecognitionVoiceMailbox?
    private var monitorTasks: [Task<Void, Never>] = []
    private var workerStopState = RecognitionWorkerStopState()
    private var startTimeNanoseconds: UInt64 = 0
    private var speechGate = RecognitionSpeechGate(
        thresholdMilliseconds: 200
    )

    public init(
        modelPath: String,
        audioProcessor: VoiceProcessingAudioProcessor,
        language: String? = nil
    ) {
        self.modelPath = modelPath
        self.audioProcessor = audioProcessor
        turnAudioProcessor = TurnAudioProcessor()
        self.language = language
    }

    public func setEventHandler(_ handler: EventHandler?) async {
        await eventRelay.setHandler(handler)
    }

    public func setFailureHandler(_ handler: FailureHandler?) async {
        await eventRelay.setFailureHandler(handler)
    }

    static func transcriptionDecodingOptions(language: String?) -> DecodingOptions {
        DecodingOptions(
            task: .transcribe,
            language: language,
            usePrefillPrompt: true,
            detectLanguage: language == nil,
            skipSpecialTokens: true,
            withoutTimestamps: true
        )
    }

    public func prepare(configuration: SidecarConfiguration) async throws {
        guard transcriptionTask == nil,
            preparedConfiguration == nil
        else {
            throw SidecarServiceFailure(
                stage: .speechRecognizer,
                code: .invalidState
            )
        }
        if let language, !Constants.languages.values.contains(language) {
            throw SidecarServiceFailure(
                stage: .speechRecognizer,
                code: .recognitionFailed
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

        let tokenizer: OfflineWhisperTokenizer
        let whisperKit: WhisperKit
        do {
            tokenizer = try await OfflineWhisperTokenizer.load(
                from: modelURL
            )
            whisperKit = try await WhisperKit(
                modelFolder: modelURL.path,
                audioProcessor: turnAudioProcessor,
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
        let pipeline = OrderedRecognitionBatchPipeline(capacity: 8)
        turnAudioProcessor.reset()
        turnAudioProcessor.setFailureHandler {
            pipeline.fail(
                SidecarServiceFailure(
                    stage: .speechRecognizer,
                    code: .recognitionFailed
                )
            )
        }
        let components = TranscriberComponents(
            audioEncoder: whisperKit.audioEncoder,
            featureExtractor: whisperKit.featureExtractor,
            segmentSeeker: whisperKit.segmentSeeker,
            textDecoder: whisperKit.textDecoder,
            tokenizer: tokenizer
        )
        let transcriber = Self.makeTranscriber(
            components: components,
            audioProcessor: turnAudioProcessor,
            language: language,
            mapper: mapper,
            pipeline: pipeline
        )
        preparedConfiguration = configuration
        transcriberComponents = components
        hypothesisPipeline = pipeline
        self.transcriber = transcriber
    }

    public func start(configuration: SidecarConfiguration) async throws {
        guard transcriptionTask == nil,
            preparedConfiguration == configuration,
            let pipeline = hypothesisPipeline,
            let transcriber
        else {
            throw SidecarServiceFailure(
                stage: .speechRecognizer,
                code: .invalidState
            )
        }

        workerStopState = RecognitionWorkerStopState()
        speechGate = RecognitionSpeechGate(
            thresholdMilliseconds: configuration.speechStartMilliseconds
        )
        startTimeNanoseconds = DispatchTime.now().uptimeNanoseconds

        let eventRelay = eventRelay
        let sessionID = configuration.sessionID
        let hypothesisWorker = pipeline.start { hypotheses in
            for hypothesis in hypotheses {
                _ = try await eventRelay.emit(
                    .hypothesis(hypothesis)
                )
            }
        }
        monitorTasks.append(
            monitorRecognitionWorker(hypothesisWorker) { failure in
                await eventRelay.reportFailure(
                    sessionID: sessionID,
                    failure: failure
                )
            }
        )
        audioProcessor.setConversionFailureHandler { failure in
            pipeline.fail(failure)
        }

        let voiceMailbox = RecognitionVoiceMailbox(capacity: 8)
        self.voiceMailbox = voiceMailbox
        audioProcessor.setVoiceWindowHandler { window in
            voiceMailbox.enqueue(.window(window))
        }
        audioProcessor.setDiscontinuityHandler {
            voiceMailbox.enqueue(.discontinuity)
        }
        let stopState = workerStopState
        let recognition = self
        let voiceWorker = Task {
            await runRecognitionWorker(stopState: stopState) {
                for await _ in voiceMailbox.stream {
                    while let input = voiceMailbox.dequeue() {
                        try await recognition.processVoiceInput(input)
                    }
                }
            }
        }
        voiceWindowTask = voiceWorker
        monitorTasks.append(
            monitorRecognitionWorker(voiceWorker) { failure in
                await eventRelay.reportFailure(
                    sessionID: sessionID,
                    failure: failure
                )
            }
        )

        try audioProcessor.startRecordingLive(
            inputDeviceID: nil
        ) { [turnAudioProcessor] samples in
            turnAudioProcessor.append(samples)
        }
        startTranscriptionWorker(
            transcriber: transcriber,
            stopState: stopState,
            sessionID: sessionID
        )
        await audioProcessor.waitUntilRecordingStarted()
    }

    public func stop() async {
        workerStopState.beginStopping()
        recognitionResetTask?.cancel()
        recognitionResetTask = nil
        turnAudioProcessor.cancelTransition()
        audioProcessor.setVoiceWindowHandler(nil)
        audioProcessor.setDiscontinuityHandler(nil)
        voiceMailbox?.finish()
        voiceMailbox = nil
        voiceWindowTask?.cancel()
        if let voiceWindowTask {
            _ = await voiceWindowTask.value
        }
        self.voiceWindowTask = nil

        await hypothesisPipeline?.cancel()
        hypothesisPipeline = nil
        if let transcriber {
            await transcriber.stopStreamTranscription()
        }
        transcriptionTask?.cancel()
        if let transcriptionTask {
            _ = await transcriptionTask.value
        }
        self.transcriptionTask = nil
        transcriptionMonitorTask?.cancel()
        if let transcriptionMonitorTask {
            _ = await transcriptionMonitorTask.value
        }
        self.transcriptionMonitorTask = nil
        self.transcriber = nil
        transcriberComponents = nil
        audioProcessor.stopRecording()
        turnAudioProcessor.setFailureHandler(nil)
        turnAudioProcessor.reset()
        preparedConfiguration = nil

        for task in monitorTasks {
            task.cancel()
        }
        monitorTasks = []
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

    private static func makeTranscriber(
        components: TranscriberComponents,
        audioProcessor: TurnAudioProcessor,
        language: String?,
        mapper: RecognitionMapper,
        pipeline: OrderedRecognitionBatchPipeline
    ) -> AudioStreamTranscriber {
        AudioStreamTranscriber(
            audioEncoder: components.audioEncoder,
            featureExtractor: components.featureExtractor,
            segmentSeeker: components.segmentSeeker,
            textDecoder: components.textDecoder,
            tokenizer: components.tokenizer,
            audioProcessor: audioProcessor,
            decodingOptions: transcriptionDecodingOptions(language: language),
            requiredSegmentsForConfirmation: 0,
            stateChangeCallback: { oldState, newState in
                let hypotheses = mapper.changes(
                    from: snapshot(oldState),
                    to: snapshot(newState)
                )
                guard !hypotheses.isEmpty else {
                    return
                }
                pipeline.enqueue(hypotheses)
            }
        )
    }

    private func startTranscriptionWorker(
        transcriber: AudioStreamTranscriber,
        stopState: RecognitionWorkerStopState,
        sessionID: UInt64
    ) {
        let eventRelay = eventRelay
        let worker = Task {
            await runRecognitionWorker(stopState: stopState) {
                try await transcriber.startStreamTranscription()
            }
        }
        transcriptionTask = worker
        transcriptionMonitorTask = monitorRecognitionWorker(worker) { failure in
            await eventRelay.reportFailure(
                sessionID: sessionID,
                failure: failure
            )
        }
    }

    private func scheduleRecognitionReset() {
        guard let configuration = preparedConfiguration else {
            return
        }
        recognitionResetTask?.cancel()
        recognitionResetTask = Task { [weak self] in
            do {
                try await Task.sleep(
                    nanoseconds: configuration.finalSilenceMilliseconds
                        * 1_000_000
                )
            } catch {
                return
            }
            guard !Task.isCancelled else {
                return
            }
            await self?.resetRecognition(
                configuration: configuration,
                discardingBufferedAudio: false
            )
        }
    }

    private func resetRecognition(
        configuration: SidecarConfiguration,
        discardingBufferedAudio: Bool
    ) async {
        guard preparedConfiguration == configuration,
            !speechGate.isSpeaking,
            !workerStopState.isStopping,
            let components = transcriberComponents,
            let pipeline = hypothesisPipeline,
            let transcriber,
            let transcriptionTask,
            workerStopState.beginRestart()
        else {
            return
        }
        recognitionResetTask = nil
        if discardingBufferedAudio {
            turnAudioProcessor.beginDiscontinuityTransition()
        } else {
            turnAudioProcessor.beginTransition()
        }
        await transcriber.stopStreamTranscription()
        _ = await transcriptionTask.value
        self.transcriptionTask = nil
        transcriptionMonitorTask?.cancel()
        if let transcriptionMonitorTask {
            _ = await transcriptionMonitorTask.value
        }
        self.transcriptionMonitorTask = nil
        guard preparedConfiguration == configuration,
            !workerStopState.isStopping
        else {
            turnAudioProcessor.cancelTransition()
            workerStopState.finishRestart()
            return
        }
        let replacement = Self.makeTranscriber(
            components: components,
            audioProcessor: turnAudioProcessor,
            language: language,
            mapper: mapper,
            pipeline: pipeline
        )
        self.transcriber = replacement
        workerStopState.finishRestart()
        startTranscriptionWorker(
            transcriber: replacement,
            stopState: workerStopState,
            sessionID: configuration.sessionID
        )
    }

    private func processVoiceInput(
        _ input: RecognitionVoiceInput
    ) async throws {
        switch input {
        case .discontinuity:
            recognitionResetTask?.cancel()
            recognitionResetTask = nil
            _ = try await eventRelay.emit(
                .captureDiscontinuity(
                    atMilliseconds: elapsedMilliseconds()
                )
            )
            speechGate.resetForDiscontinuity()
            if let configuration = preparedConfiguration {
                await resetRecognition(
                    configuration: configuration,
                    discardingBufferedAudio: true
                )
            }
        case .window(let window):
            try await processVoiceWindow(window)
        }
    }

    private func processVoiceWindow(_ window: [Float]) async throws {
        let isSpeech = vad.voiceActivity(in: window).last == true
        if isSpeech {
            recognitionResetTask?.cancel()
            recognitionResetTask = nil
        }
        let atMilliseconds = elapsedMilliseconds()
        _ = try await eventRelay.emit(
            .voiceWindow(
                isSpeech: isSpeech,
                frameMilliseconds: 100,
                atMilliseconds: atMilliseconds
            )
        )

        switch speechGate.observe(isSpeech: isSpeech) {
        case .none:
            break
        case .started:
            _ = try await eventRelay.emit(
                .activity(
                    .speechStarted(
                        atMilliseconds: atMilliseconds
                    )
                )
            )
        case .continued:
            _ = try await eventRelay.emit(
                .activity(
                    .speechContinued(
                        atMilliseconds: atMilliseconds
                    )
                )
            )
        case .ended:
            _ = try await eventRelay.emit(
                .activity(
                    .speechEnded(
                        atMilliseconds: atMilliseconds
                    )
                )
            )
            scheduleRecognitionReset()
        }
    }

    private func elapsedMilliseconds() -> UInt64 {
        (DispatchTime.now().uptimeNanoseconds
            &- startTimeNanoseconds) / 1_000_000
    }
}
