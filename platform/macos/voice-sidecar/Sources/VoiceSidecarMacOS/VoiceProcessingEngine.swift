@preconcurrency import AVFoundation
import Darwin
@preconcurrency import Dispatch
import Foundation
import VoiceSidecarCore

public enum MicrophoneAuthorization: Equatable, Sendable {
    case authorized
    case denied
    case restricted
    case notDetermined
}

public protocol MicrophonePermissionProviding: Sendable {
    func authorizationStatus() -> MicrophoneAuthorization
    func requestAccess() async -> Bool
}

public protocol PCMPlaybackScheduling: Sendable {
    var playbackFormat: AVAudioFormat { get }

    func schedulePlayback(
        _ buffer: AVAudioPCMBuffer,
        completion: @escaping @Sendable () -> Void
    ) throws

    func resetPlayback()
}

public enum CaptureRingPushResult: Equatable, Sendable {
    case enqueued
    case capacityOverflow
    case formatMismatch
    case oversizedInput
    case invalidBuffer
}

public enum CapturePCMEvent: @unchecked Sendable {
    case buffer(AVAudioPCMBuffer)
    case discontinuity
}

enum CaptureStructuralFault: Int32, Equatable, Sendable {
    case formatMismatch = 1
    case oversizedInput = 2
    case invalidBuffer = 3
}

private struct SystemMicrophonePermissionProvider: MicrophonePermissionProviding {
    func authorizationStatus() -> MicrophoneAuthorization {
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized:
            .authorized
        case .denied:
            .denied
        case .restricted:
            .restricted
        case .notDetermined:
            .notDetermined
        @unknown default:
            .denied
        }
    }

    func requestAccess() async -> Bool {
        await AVCaptureDevice.requestAccess(for: .audio)
    }
}

public final class CapturePCMBufferRing: @unchecked Sendable {
    private final class Slot {
        let buffer: AVAudioPCMBuffer
        var discontinuityBefore = false

        init(buffer: AVAudioPCMBuffer) {
            self.buffer = buffer
        }
    }

    private struct FormatSignature: Equatable {
        let commonFormat: AVAudioCommonFormat
        let sampleRate: Double
        let channelCount: AVAudioChannelCount
        let isInterleaved: Bool

        init(_ format: AVAudioFormat) {
            commonFormat = format.commonFormat
            sampleRate = format.sampleRate
            channelCount = format.channelCount
            isInterleaved = format.isInterleaved
        }
    }

    private let formatSignature: FormatSignature
    private let frameCapacity: AVAudioFrameCount
    private let capacity: Int
    private let slots: [Slot]
    private var writeSequence: Int64 = 0
    private var readSequence: Int64 = 0
    private var droppedBuffers: Int64 = 0
    private var pendingDiscontinuity: Int32 = 0
    private var maximumQueuedBuffers: Int64 = 0

    public var droppedBufferCount: Int {
        Int(OSAtomicAdd64Barrier(0, &droppedBuffers))
    }

    public var queuedBufferCount: Int {
        let write = OSAtomicAdd64Barrier(0, &writeSequence)
        let read = OSAtomicAdd64Barrier(0, &readSequence)
        return Int(write - read)
    }

    public var maximumQueuedBufferCount: Int {
        Int(OSAtomicAdd64Barrier(0, &maximumQueuedBuffers))
    }

    public init(
        format: AVAudioFormat,
        frameCapacity: AVAudioFrameCount,
        capacity: Int
    ) throws {
        guard frameCapacity > 0, capacity > 0 else {
            throw PCMConversionError.invalidBuffer
        }
        var slots: [Slot] = []
        slots.reserveCapacity(capacity)
        for _ in 0..<capacity {
            guard
                let buffer = AVAudioPCMBuffer(
                    pcmFormat: format,
                    frameCapacity: frameCapacity
                )
            else {
                throw PCMConversionError.invalidBuffer
            }
            slots.append(Slot(buffer: buffer))
        }
        formatSignature = FormatSignature(format)
        self.frameCapacity = frameCapacity
        self.capacity = capacity
        self.slots = slots
    }

    @discardableResult
    public func copyFromTap(
        _ source: AVAudioPCMBuffer
    ) -> CaptureRingPushResult {
        guard FormatSignature(source.format) == formatSignature else {
            OSAtomicIncrement64Barrier(&droppedBuffers)
            return .formatMismatch
        }
        guard source.frameLength <= frameCapacity else {
            OSAtomicIncrement64Barrier(&droppedBuffers)
            return .oversizedInput
        }

        let write = OSAtomicAdd64Barrier(0, &writeSequence)
        let read = OSAtomicAdd64Barrier(0, &readSequence)
        guard write - read < Int64(capacity) else {
            OSAtomicIncrement64Barrier(&droppedBuffers)
            _ = OSAtomicCompareAndSwap32Barrier(
                0,
                1,
                &pendingDiscontinuity
            )
            return .capacityOverflow
        }

        let slot = slots[Int(write % Int64(capacity))]
        let destination = slot.buffer
        destination.frameLength = source.frameLength
        let sourceList = UnsafeMutableAudioBufferListPointer(
            source.mutableAudioBufferList
        )
        let destinationList = UnsafeMutableAudioBufferListPointer(
            destination.mutableAudioBufferList
        )
        guard sourceList.count == destinationList.count else {
            OSAtomicIncrement64Barrier(&droppedBuffers)
            return .invalidBuffer
        }
        for index in sourceList.indices {
            guard let sourceData = sourceList[index].mData,
                let destinationData = destinationList[index].mData,
                sourceList[index].mDataByteSize
                    <= destinationList[index].mDataByteSize
            else {
                OSAtomicIncrement64Barrier(&droppedBuffers)
                return .invalidBuffer
            }
            memcpy(
                destinationData,
                sourceData,
                Int(sourceList[index].mDataByteSize)
            )
            destinationList[index].mDataByteSize =
                sourceList[index].mDataByteSize
        }
        slot.discontinuityBefore = OSAtomicCompareAndSwap32Barrier(
            1,
            0,
            &pendingDiscontinuity
        )
        OSAtomicIncrement64Barrier(&writeSequence)
        recordMaximumQueued(write - read + 1)
        return .enqueued
    }

    public func drain(
        _ handler: (CapturePCMEvent) -> Void
    ) {
        var read = OSAtomicAdd64Barrier(0, &readSequence)
        let write = OSAtomicAdd64Barrier(0, &writeSequence)
        while read < write {
            let slot = slots[Int(read % Int64(capacity))]
            if slot.discontinuityBefore {
                slot.discontinuityBefore = false
                handler(.discontinuity)
            }
            handler(.buffer(slot.buffer))
            read += 1
            OSAtomicIncrement64Barrier(&readSequence)
        }
    }

    private func recordMaximumQueued(_ value: Int64) {
        var current = OSAtomicAdd64Barrier(0, &maximumQueuedBuffers)
        while value > current {
            if OSAtomicCompareAndSwap64Barrier(
                current,
                value,
                &maximumQueuedBuffers
            ) {
                return
            }
            current = OSAtomicAdd64Barrier(0, &maximumQueuedBuffers)
        }
    }
}

final class CaptureBufferPump: @unchecked Sendable {
    typealias EventHandler = @Sendable (CapturePCMEvent) -> Void
    typealias FaultHandler = @Sendable (CaptureStructuralFault) -> Void

    private let ring: CapturePCMBufferRing
    private let eventHandler: EventHandler
    private let faultHandler: FaultHandler
    private let semaphore = DispatchSemaphore(value: 0)
    private let worker = DispatchGroup()
    private let queue = DispatchQueue(
        label: "conversation-runtime.voice-capture"
    )
    private var active: Int64 = 1
    private var pendingFault: Int32 = 0

    init(
        ring: CapturePCMBufferRing,
        eventHandler: @escaping EventHandler,
        faultHandler: @escaping FaultHandler
    ) {
        self.ring = ring
        self.eventHandler = eventHandler
        self.faultHandler = faultHandler
        worker.enter()
        queue.async { [self] in
            defer { worker.leave() }
            run()
        }
    }

    @discardableResult
    func enqueue(_ buffer: AVAudioPCMBuffer) -> CaptureRingPushResult {
        let result = ring.copyFromTap(buffer)
        switch result {
        case .enqueued, .capacityOverflow:
            semaphore.signal()
        case .formatMismatch:
            recordFault(.formatMismatch)
        case .oversizedInput:
            recordFault(.oversizedInput)
        case .invalidBuffer:
            recordFault(.invalidBuffer)
        }
        return result
    }

    func stop() {
        _ = OSAtomicCompareAndSwap64Barrier(1, 0, &active)
        semaphore.signal()
        worker.wait()
    }

    private func run() {
        while true {
            semaphore.wait()
            guard OSAtomicAdd64Barrier(0, &active) == 1 else {
                return
            }
            if let fault = takeFault() {
                _ = OSAtomicCompareAndSwap64Barrier(1, 0, &active)
                DispatchQueue.global().async { [faultHandler] in
                    faultHandler(fault)
                }
                return
            }
            ring.drain(eventHandler)
        }
    }

    private func recordFault(_ fault: CaptureStructuralFault) {
        _ = OSAtomicCompareAndSwap32Barrier(
            0,
            fault.rawValue,
            &pendingFault
        )
        semaphore.signal()
    }

    private func takeFault() -> CaptureStructuralFault? {
        let value = OSAtomicAdd32Barrier(0, &pendingFault)
        guard value != 0,
            OSAtomicCompareAndSwap32Barrier(value, 0, &pendingFault)
        else {
            return nil
        }
        return CaptureStructuralFault(rawValue: value)
    }
}

public final class VoiceProcessingEngine:
    SidecarAudioService,
    PCMPlaybackScheduling,
    @unchecked Sendable
{
    private static let captureFrameCapacity: AVAudioFrameCount = 8_192

    public typealias FailureHandler =
        @Sendable (
            UInt64,
            SidecarServiceFailure
        ) async -> Void

    private struct FormatSignature: Equatable {
        let sampleRate: Double
        let channelCount: AVAudioChannelCount

        init(_ format: AVAudioFormat) {
            sampleRate = format.sampleRate
            channelCount = format.channelCount
        }
    }

    private let engine = AVAudioEngine()
    private let player = AVAudioPlayerNode()
    private let permissionProvider: any MicrophonePermissionProviding
    private let stateLock = NSLock()

    private var captureHandler: (@Sendable (CapturePCMEvent) -> Void)?
    private var capturePump: CaptureBufferPump?
    private var failureHandler: FailureHandler?
    private var configurationObserver: NSObjectProtocol?
    private var sessionID: UInt64?
    private var inputSignature: FormatSignature?
    private var outputSignature: FormatSignature?
    private var running = false
    private var captureActive = false
    private var resolvedPlaybackFormat: AVAudioFormat

    public var playbackFormat: AVAudioFormat {
        stateLock.withLock {
            resolvedPlaybackFormat
        }
    }

    public var isRunning: Bool {
        stateLock.withLock {
            running
        }
    }

    public var isCaptureActive: Bool {
        stateLock.withLock {
            captureActive
        }
    }

    public convenience init() {
        self.init(
            permissionProvider: SystemMicrophonePermissionProvider()
        )
    }

    public init(permissionProvider: any MicrophonePermissionProviding) {
        self.permissionProvider = permissionProvider
        resolvedPlaybackFormat = AVAudioFormat(
            standardFormatWithSampleRate: 48_000,
            channels: 2
        )!
    }

    deinit {
        capturePump?.stop()
        if let configurationObserver {
            NotificationCenter.default.removeObserver(configurationObserver)
        }
    }

    public static func requireMicrophonePermission(
        using provider: any MicrophonePermissionProviding
    ) async throws {
        switch provider.authorizationStatus() {
        case .authorized:
            return
        case .denied, .restricted:
            throw permissionFailure()
        case .notDetermined:
            guard await provider.requestAccess() else {
                throw permissionFailure()
            }
        }
    }

    static func configurePlaybackGraph(
        engine: AVAudioEngine,
        player: AVAudioPlayerNode,
        sampleRate: Double
    ) throws -> AVAudioFormat {
        guard sampleRate.isFinite, sampleRate > 0,
            let playbackFormat = AVAudioFormat(
                standardFormatWithSampleRate: sampleRate,
                channels: 1
            )
        else {
            throw SidecarServiceFailure(
                stage: .audioOutput,
                code: .playbackFailed
            )
        }
        if !engine.attachedNodes.contains(player) {
            engine.attach(player)
        }
        engine.connect(
            player,
            to: engine.mainMixerNode,
            format: playbackFormat
        )
        engine.connect(
            engine.mainMixerNode,
            to: engine.outputNode,
            format: playbackFormat
        )
        return playbackFormat
    }

    static func makeCaptureRing(
        format: AVAudioFormat
    ) throws -> CapturePCMBufferRing {
        try CapturePCMBufferRing(
            format: format,
            frameCapacity: captureFrameCapacity,
            capacity: 8
        )
    }

    static func startupFailure(
        from error: any Error
    ) -> SidecarServiceFailure {
        if let failure = error as? SidecarServiceFailure {
            return failure
        }
        return SidecarServiceFailure(
            stage: .audioCapture,
            code: .audioDeviceUnavailable
        )
    }

    public func setCaptureHandler(
        _ handler: (@Sendable (CapturePCMEvent) -> Void)?
    ) {
        stateLock.withLock {
            captureHandler = handler
        }
    }

    public func setFailureHandler(_ handler: FailureHandler?) {
        stateLock.withLock {
            failureHandler = handler
        }
    }

    public func start(configuration: SidecarConfiguration) async throws {
        try await Self.requireMicrophonePermission(using: permissionProvider)

        let alreadyRunning = stateLock.withLock {
            running
        }
        guard !alreadyRunning else {
            throw SidecarServiceFailure(
                stage: .audioCapture,
                code: .invalidState
            )
        }

        let inputNode = engine.inputNode
        let inputFormat = inputNode.inputFormat(forBus: 0)
        let outputFormat = engine.outputNode.inputFormat(forBus: 0)
        try Self.requireUsable(inputFormat)
        try Self.requireUsable(outputFormat)

        do {
            try inputNode.setVoiceProcessingEnabled(true)
            let playbackFormat = try Self.configurePlaybackGraph(
                engine: engine,
                player: player,
                sampleRate: outputFormat.sampleRate
            )
            let ring = try Self.makeCaptureRing(format: inputFormat)
            let pump = CaptureBufferPump(
                ring: ring,
                eventHandler: { [weak self] event in
                    self?.deliverCapture(event)
                },
                faultHandler: { [weak self] _ in
                    self?.captureFault()
                }
            )
            capturePump = pump
            inputNode.installTap(
                onBus: 0,
                bufferSize: Self.captureFrameCapacity,
                format: inputFormat
            ) { buffer, _ in
                pump.enqueue(buffer)
            }
            engine.prepare()
            try engine.start()
            player.play()

            stateLock.withLock {
                sessionID = configuration.sessionID
                inputSignature = FormatSignature(inputFormat)
                outputSignature = FormatSignature(outputFormat)
                resolvedPlaybackFormat = playbackFormat
                running = true
                captureActive = true
            }
            installConfigurationObserver()
        } catch {
            inputNode.removeTap(onBus: 0)
            capturePump?.stop()
            capturePump = nil
            player.stop()
            engine.stop()
            throw Self.startupFailure(from: error)
        }
    }

    public func pauseCapture() async throws {
        let pump = try stateLock.withLock {
            guard running, captureActive, let capturePump else {
                throw SidecarServiceFailure(
                    stage: .audioCapture,
                    code: .invalidState
                )
            }
            captureActive = false
            self.capturePump = nil
            return capturePump
        }
        engine.inputNode.removeTap(onBus: 0)
        pump.stop()
    }

    public func resumeCapture() async throws {
        let expectedInput = try stateLock.withLock {
            guard running, !captureActive, let inputSignature else {
                throw SidecarServiceFailure(
                    stage: .audioCapture,
                    code: .invalidState
                )
            }
            return inputSignature
        }
        let inputNode = engine.inputNode
        let inputFormat = inputNode.inputFormat(forBus: 0)
        try Self.requireUsable(inputFormat)
        guard FormatSignature(inputFormat) == expectedInput else {
            throw SidecarServiceFailure(
                stage: .audioCapture,
                code: .audioDeviceUnavailable
            )
        }
        let ring = try Self.makeCaptureRing(format: inputFormat)
        let pump = CaptureBufferPump(
            ring: ring,
            eventHandler: { [weak self] event in
                self?.deliverCapture(event)
            },
            faultHandler: { [weak self] _ in
                self?.captureFault()
            }
        )
        inputNode.installTap(
            onBus: 0,
            bufferSize: Self.captureFrameCapacity,
            format: inputFormat
        ) { buffer, _ in
            pump.enqueue(buffer)
        }
        do {
            try stateLock.withLock {
                guard running, !captureActive else {
                    throw SidecarServiceFailure(
                        stage: .audioCapture,
                        code: .invalidState
                    )
                }
                capturePump = pump
                captureActive = true
            }
        } catch {
            inputNode.removeTap(onBus: 0)
            pump.stop()
            throw error
        }
    }

    public func stop() async {
        let (observer, pump, removeCaptureTap) = stateLock.withLock {
            let value = configurationObserver
            configurationObserver = nil
            running = false
            let removeCaptureTap = captureActive
            captureActive = false
            sessionID = nil
            inputSignature = nil
            outputSignature = nil
            captureHandler = nil
            let pump = capturePump
            capturePump = nil
            return (value, pump, removeCaptureTap)
        }
        if let observer {
            NotificationCenter.default.removeObserver(observer)
        }
        if removeCaptureTap {
            engine.inputNode.removeTap(onBus: 0)
        }
        pump?.stop()
        player.stop()
        player.reset()
        engine.stop()
    }

    public func schedulePlayback(
        _ buffer: AVAudioPCMBuffer,
        completion: @escaping @Sendable () -> Void
    ) throws {
        guard isRunning else {
            throw SidecarServiceFailure(
                stage: .audioOutput,
                code: .playbackFailed
            )
        }
        player.scheduleBuffer(
            buffer,
            completionCallbackType: .dataPlayedBack
        ) { _ in
            completion()
        }
        if !player.isPlaying {
            player.play()
        }
    }

    public func resetPlayback() {
        player.stop()
        player.reset()
        if isRunning {
            player.play()
        }
    }

    private static func permissionFailure() -> SidecarServiceFailure {
        SidecarServiceFailure(
            stage: .audioCapture,
            code: .permissionDenied
        )
    }

    private static func requireUsable(_ format: AVAudioFormat) throws {
        guard format.sampleRate > 0, format.channelCount > 0 else {
            throw SidecarServiceFailure(
                stage: .audioCapture,
                code: .audioDeviceUnavailable
            )
        }
    }

    private func deliverCapture(_ event: CapturePCMEvent) {
        let handler = stateLock.withLock {
            captureHandler
        }
        handler?(event)
    }

    private func installConfigurationObserver() {
        let observer = NotificationCenter.default.addObserver(
            forName: .AVAudioEngineConfigurationChange,
            object: engine,
            queue: nil
        ) { [weak self] _ in
            self?.configurationChanged()
        }
        stateLock.withLock {
            configurationObserver = observer
        }
    }

    private func configurationChanged() {
        captureFault()
    }

    private func captureFault() {
        let fatal: (UInt64, FailureHandler?, CaptureBufferPump?, Bool)? =
            stateLock.withLock {
                guard running, let sessionID else {
                    return nil
                }
                running = false
                let removeCaptureTap = captureActive
                captureActive = false
                let pump = capturePump
                capturePump = nil
                return (sessionID, failureHandler, pump, removeCaptureTap)
            }
        guard let (sessionID, handler, pump, removeCaptureTap) = fatal else {
            return
        }
        if removeCaptureTap {
            engine.inputNode.removeTap(onBus: 0)
        }
        pump?.stop()
        player.stop()
        engine.stop()
        Task {
            await handler?(
                sessionID,
                SidecarServiceFailure(
                    stage: .audioCapture,
                    code: .audioDeviceUnavailable
                )
            )
        }
    }
}
