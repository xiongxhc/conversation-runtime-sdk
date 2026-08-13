import Foundation
import SherpaOnnx
import VoiceSidecarCore
@preconcurrency import WhisperKit

public protocol SidecarRecognitionEventSource: SidecarRecognitionService {
    func setEventHandler(_ handler: WhisperKitRecognition.EventHandler?) async
    func setFailureHandler(_ handler: WhisperKitRecognition.FailureHandler?) async
}

extension WhisperKitRecognition: SidecarRecognitionEventSource {}
extension SenseVoiceRecognition: SidecarRecognitionEventSource {}

final class SenseVoiceEngine: @unchecked Sendable {
    private let recognizer: SherpaOnnxOfflineRecognizer

    init(modelFilePath: String, tokensFilePath: String, language: String) {
        var config = sherpaOnnxOfflineRecognizerConfig(
            featConfig: sherpaOnnxFeatureConfig(
                sampleRate: 16_000,
                featureDim: 80
            ),
            modelConfig: sherpaOnnxOfflineModelConfig(
                tokens: tokensFilePath,
                numThreads: 2,
                senseVoice: sherpaOnnxOfflineSenseVoiceModelConfig(
                    model: modelFilePath,
                    language: language,
                    useInverseTextNormalization: true
                )
            )
        )
        recognizer = SherpaOnnxOfflineRecognizer(config: &config)
    }

    // Serialized by construction: only the single decode worker (and the
    // model-gated tests) call this.
    func transcribe(_ samples: [Float]) -> String {
        recognizer.decode(samples: samples, sampleRate: 16_000).text
    }
}

// Whole-buffer offline decoding has no per-segment alignment, so segment
// bookkeeping is sample-offset based: each engine-final decode advances the
// finalized prefix and the next segment decodes only the audio after it.
struct SenseVoiceSegmentTracker: Sendable {
    private(set) var segmentID: UInt64 = 1
    private(set) var finalizedSampleCount = 0
    private var lastPartialText: String?

    mutating func partialHypothesis(text: String) -> RecognitionHypothesis? {
        guard isEmittable(text), text != lastPartialText else {
            return nil
        }
        lastPartialText = text
        return RecognitionHypothesis(
            segmentID: segmentID,
            text: text,
            engineFinal: false
        )
    }

    mutating func finalHypothesis(
        text: String,
        decodedThrough sampleCount: Int
    ) -> RecognitionHypothesis? {
        finalizedSampleCount = max(finalizedSampleCount, sampleCount)
        lastPartialText = nil
        guard isEmittable(text) else {
            return nil
        }
        let hypothesis = RecognitionHypothesis(
            segmentID: segmentID,
            text: text,
            engineFinal: true
        )
        segmentID += 1
        return hypothesis
    }

    mutating func resetForNewTurn() {
        self = SenseVoiceSegmentTracker()
    }

    private func isEmittable(_ text: String) -> Bool {
        !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && containsRecognizableContent(text)
    }
}

enum SenseVoiceDecodeRequest: Equatable, Sendable {
    case partial
    case final
    case reset
}

final class SenseVoiceDecodeMailbox: @unchecked Sendable {
    private let lock = NSLock()
    private let notifications: AsyncStream<Void>
    private let continuation: AsyncStream<Void>.Continuation
    private var requests: [SenseVoiceDecodeRequest] = []
    private var finished = false

    var stream: AsyncStream<Void> {
        notifications
    }

    init() {
        (notifications, continuation) = AsyncStream.makeStream(
            bufferingPolicy: .bufferingNewest(1)
        )
    }

    func enqueue(_ request: SenseVoiceDecodeRequest) {
        let shouldNotify = lock.withLock {
            guard !finished else {
                return false
            }
            if request == .partial, requests.last == .partial {
                return false
            }
            requests.append(request)
            return true
        }
        if shouldNotify {
            continuation.yield()
        }
    }

    func dequeue() -> SenseVoiceDecodeRequest? {
        lock.withLock {
            guard !requests.isEmpty else {
                return nil
            }
            return requests.removeFirst()
        }
    }

    func clear() {
        lock.withLock {
            requests.removeAll(keepingCapacity: true)
        }
    }

    func finish() {
        lock.withLock {
            finished = true
            requests.removeAll(keepingCapacity: true)
        }
        continuation.finish()
    }
}

public actor SenseVoiceRecognition: SidecarRecognitionService {
    public typealias EventHandler = WhisperKitRecognition.EventHandler
    public typealias FailureHandler = WhisperKitRecognition.FailureHandler

    static let supportedLanguages: Set<String> = [
        "auto", "zh", "en", "ja", "ko", "yue",
    ]
    private static let partialDecodeIntervalMilliseconds: UInt64 = 1_000
    private static let minimumDecodeSamples = 1_600

    private struct DecodeJob: Sendable {
        let engine: SenseVoiceEngine
        let samples: [Float]
        let snapshotCount: Int
        let epoch: UInt64
    }

    private let modelPath: String
    private let audioProcessor: VoiceProcessingAudioProcessor
    private let turnAudioProcessor: TurnAudioProcessor
    private let language: String?
    // Same capture-window VAD calibration as WhisperKitRecognition; see the
    // threshold rationale there.
    private let vad = EnergyVAD(
        sampleRate: 16_000,
        frameLength: 0.1,
        energyThreshold: 0.04
    )
    private let eventRelay = RecognitionEventRelay()

    private var preparedConfiguration: SidecarConfiguration?
    private var engine: SenseVoiceEngine?
    private var hypothesisPipeline: OrderedRecognitionBatchPipeline?
    private var voiceWindowTask: Task<RecognitionWorkerCompletion, Never>?
    private var voiceMailbox: RecognitionVoiceMailbox?
    private var decodeTask: Task<RecognitionWorkerCompletion, Never>?
    private var decodeMailbox: SenseVoiceDecodeMailbox?
    private var recognitionResetTask: Task<Void, Never>?
    private var monitorTasks: [Task<Void, Never>] = []
    private var workerStopState = RecognitionWorkerStopState()
    private var startTimeNanoseconds: UInt64 = 0
    private var speechGate = RecognitionSpeechGate(
        thresholdMilliseconds: 200
    )
    private var tracker = SenseVoiceSegmentTracker()
    private var turnEpoch: UInt64 = 0
    private var lastPartialRequestMilliseconds: UInt64 = 0

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

    public func prepare(configuration: SidecarConfiguration) async throws {
        guard decodeTask == nil,
            preparedConfiguration == nil
        else {
            throw SidecarServiceFailure(
                stage: .speechRecognizer,
                code: .invalidState
            )
        }
        let effectiveLanguage = language ?? "auto"
        guard Self.supportedLanguages.contains(effectiveLanguage) else {
            throw SidecarServiceFailure(
                stage: .speechRecognizer,
                code: .recognitionFailed
            )
        }
        let modelURL = URL(fileURLWithPath: modelPath, isDirectory: true)
        var modelIsDirectory = ObjCBool(false)
        let modelFileURL = modelURL.appendingPathComponent(
            "model.int8.onnx",
            isDirectory: false
        )
        let tokensFileURL = modelURL.appendingPathComponent(
            "tokens.txt",
            isDirectory: false
        )
        guard (modelPath as NSString).isAbsolutePath,
            FileManager.default.fileExists(
                atPath: modelURL.path,
                isDirectory: &modelIsDirectory
            ),
            modelIsDirectory.boolValue,
            FileManager.default.fileExists(atPath: modelFileURL.path),
            FileManager.default.fileExists(atPath: tokensFileURL.path)
        else {
            throw SidecarServiceFailure(
                stage: .speechRecognizer,
                code: .recognitionFailed
            )
        }

        let engine = SenseVoiceEngine(
            modelFilePath: modelFileURL.path,
            tokensFilePath: tokensFileURL.path,
            language: effectiveLanguage
        )
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
        preparedConfiguration = configuration
        self.engine = engine
        hypothesisPipeline = pipeline
    }

    public func start(configuration: SidecarConfiguration) async throws {
        guard decodeTask == nil,
            preparedConfiguration == configuration,
            let pipeline = hypothesisPipeline,
            engine != nil
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
        tracker = SenseVoiceSegmentTracker()
        lastPartialRequestMilliseconds = 0
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

        let decodeMailbox = SenseVoiceDecodeMailbox()
        self.decodeMailbox = decodeMailbox
        let decodeWorker = Task {
            await runRecognitionWorker(stopState: stopState) {
                for await _ in decodeMailbox.stream {
                    while let request = decodeMailbox.dequeue() {
                        try await recognition.process(
                            decodeRequest: request
                        )
                    }
                }
            }
        }
        decodeTask = decodeWorker
        monitorTasks.append(
            monitorRecognitionWorker(decodeWorker) { failure in
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
        try turnAudioProcessor.startRecordingLive(
            inputDeviceID: nil,
            callback: nil
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
        voiceWindowTask = nil

        decodeMailbox?.finish()
        decodeMailbox = nil
        decodeTask?.cancel()
        if let decodeTask {
            _ = await decodeTask.value
        }
        decodeTask = nil

        await hypothesisPipeline?.cancel()
        hypothesisPipeline = nil
        audioProcessor.stopRecording()
        turnAudioProcessor.setFailureHandler(nil)
        turnAudioProcessor.reset()
        engine = nil
        preparedConfiguration = nil

        for task in monitorTasks {
            task.cancel()
        }
        monitorTasks = []
    }

    // Runs on the decode worker so requests, in-flight decodes, and turn
    // resets stay strictly ordered. The blocking model call happens between
    // two actor hops so voice-window processing never stalls behind it.
    private func process(
        decodeRequest request: SenseVoiceDecodeRequest
    ) async throws {
        switch request {
        case .reset:
            try resetTurnBuffer()
        case .partial, .final:
            guard let job = beginDecode() else {
                return
            }
            let engine = job.engine
            let samples = job.samples
            let text = await Task.detached(priority: .userInitiated) {
                engine.transcribe(samples)
            }.value
            try completeDecode(request, job: job, text: text)
        }
    }

    private func beginDecode() -> DecodeJob? {
        guard let engine,
            !workerStopState.isStopping
        else {
            return nil
        }
        let samples = turnAudioProcessor.audioSamples
        let finalized = min(tracker.finalizedSampleCount, samples.count)
        let slice = Array(samples[finalized...])
        guard slice.count >= Self.minimumDecodeSamples else {
            return nil
        }
        return DecodeJob(
            engine: engine,
            samples: slice,
            snapshotCount: samples.count,
            epoch: turnEpoch
        )
    }

    private func completeDecode(
        _ request: SenseVoiceDecodeRequest,
        job: DecodeJob,
        text: String
    ) throws {
        guard job.epoch == turnEpoch,
            !workerStopState.isStopping,
            let pipeline = hypothesisPipeline
        else {
            return
        }
        let hypothesis: RecognitionHypothesis?
        switch request {
        case .partial:
            hypothesis = tracker.partialHypothesis(text: text)
        case .final:
            hypothesis = tracker.finalHypothesis(
                text: text,
                decodedThrough: job.snapshotCount
            )
        case .reset:
            return
        }
        if let hypothesis {
            pipeline.enqueue([hypothesis])
        }
    }

    // Mirrors WhisperKitRecognition's transcriber replacement after final
    // silence: the turn buffer restarts from bounded pre-roll and segment
    // numbering begins again at 1.
    private func resetTurnBuffer() throws {
        guard preparedConfiguration != nil,
            !speechGate.isSpeaking,
            !workerStopState.isStopping
        else {
            return
        }
        turnEpoch += 1
        tracker.resetForNewTurn()
        lastPartialRequestMilliseconds = 0
        turnAudioProcessor.beginTransition()
        try turnAudioProcessor.startRecordingLive(
            inputDeviceID: nil,
            callback: nil
        )
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
            await self?.requestTurnReset()
        }
    }

    private func requestTurnReset() {
        recognitionResetTask = nil
        decodeMailbox?.enqueue(.reset)
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
            turnEpoch += 1
            tracker.resetForNewTurn()
            lastPartialRequestMilliseconds = 0
            decodeMailbox?.clear()
            turnAudioProcessor.beginDiscontinuityTransition()
            try turnAudioProcessor.startRecordingLive(
                inputDeviceID: nil,
                callback: nil
            )
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
            lastPartialRequestMilliseconds = atMilliseconds
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
            if atMilliseconds &- lastPartialRequestMilliseconds
                >= Self.partialDecodeIntervalMilliseconds
            {
                lastPartialRequestMilliseconds = atMilliseconds
                decodeMailbox?.enqueue(.partial)
            }
        case .ended:
            _ = try await eventRelay.emit(
                .activity(
                    .speechEnded(
                        atMilliseconds: atMilliseconds
                    )
                )
            )
            decodeMailbox?.enqueue(.final)
            scheduleRecognitionReset()
        }
    }

    private func elapsedMilliseconds() -> UInt64 {
        (DispatchTime.now().uptimeNanoseconds
            &- startTimeNanoseconds) / 1_000_000
    }
}
