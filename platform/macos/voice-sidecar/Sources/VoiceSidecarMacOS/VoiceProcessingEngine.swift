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
    private let buffers: [AVAudioPCMBuffer]
    private var writeSequence: Int64 = 0
    private var readSequence: Int64 = 0
    private var droppedBuffers: Int64 = 0

    public var droppedBufferCount: Int {
        Int(OSAtomicAdd64Barrier(0, &droppedBuffers))
    }

    public init(
        format: AVAudioFormat,
        frameCapacity: AVAudioFrameCount,
        capacity: Int
    ) throws {
        guard frameCapacity > 0, capacity > 0 else {
            throw PCMConversionError.invalidBuffer
        }
        var buffers: [AVAudioPCMBuffer] = []
        buffers.reserveCapacity(capacity)
        for _ in 0..<capacity {
            guard
                let buffer = AVAudioPCMBuffer(
                    pcmFormat: format,
                    frameCapacity: frameCapacity
                )
            else {
                throw PCMConversionError.invalidBuffer
            }
            buffers.append(buffer)
        }
        formatSignature = FormatSignature(format)
        self.frameCapacity = frameCapacity
        self.capacity = capacity
        self.buffers = buffers
    }

    @discardableResult
    public func copyFromTap(_ source: AVAudioPCMBuffer) -> Bool {
        guard FormatSignature(source.format) == formatSignature,
            source.frameLength <= frameCapacity
        else {
            OSAtomicIncrement64Barrier(&droppedBuffers)
            return false
        }

        let write = OSAtomicAdd64Barrier(0, &writeSequence)
        let read = OSAtomicAdd64Barrier(0, &readSequence)
        guard write - read < Int64(capacity) else {
            OSAtomicIncrement64Barrier(&droppedBuffers)
            return false
        }

        let destination = buffers[Int(write % Int64(capacity))]
        destination.frameLength = source.frameLength
        let sourceList = UnsafeMutableAudioBufferListPointer(
            source.mutableAudioBufferList
        )
        let destinationList = UnsafeMutableAudioBufferListPointer(
            destination.mutableAudioBufferList
        )
        guard sourceList.count == destinationList.count else {
            OSAtomicIncrement64Barrier(&droppedBuffers)
            return false
        }
        for index in sourceList.indices {
            guard let sourceData = sourceList[index].mData,
                let destinationData = destinationList[index].mData,
                sourceList[index].mDataByteSize
                    <= destinationList[index].mDataByteSize
            else {
                OSAtomicIncrement64Barrier(&droppedBuffers)
                return false
            }
            memcpy(
                destinationData,
                sourceData,
                Int(sourceList[index].mDataByteSize)
            )
            destinationList[index].mDataByteSize =
                sourceList[index].mDataByteSize
        }
        OSAtomicIncrement64Barrier(&writeSequence)
        return true
    }

    public func drain(
        _ handler: (AVAudioPCMBuffer) -> Void
    ) {
        var read = OSAtomicAdd64Barrier(0, &readSequence)
        let write = OSAtomicAdd64Barrier(0, &writeSequence)
        while read < write {
            handler(buffers[Int(read % Int64(capacity))])
            read += 1
            OSAtomicIncrement64Barrier(&readSequence)
        }
    }
}

private final class CaptureBufferPump: @unchecked Sendable {
    private let ring: CapturePCMBufferRing
    private let handler: @Sendable (AVAudioPCMBuffer) -> Void
    private let semaphore = DispatchSemaphore(value: 0)
    private let queue = DispatchQueue(
        label: "conversation-runtime.voice-capture"
    )
    private var active: Int64 = 1

    init(
        ring: CapturePCMBufferRing,
        handler: @escaping @Sendable (AVAudioPCMBuffer) -> Void
    ) {
        self.ring = ring
        self.handler = handler
        queue.async { [self] in
            run()
        }
    }

    func enqueue(_ buffer: AVAudioPCMBuffer) {
        if ring.copyFromTap(buffer) {
            semaphore.signal()
        }
    }

    func stop() {
        _ = OSAtomicCompareAndSwap64Barrier(1, 0, &active)
        semaphore.signal()
    }

    private func run() {
        while true {
            semaphore.wait()
            guard OSAtomicAdd64Barrier(0, &active) == 1 else {
                return
            }
            ring.drain(handler)
        }
    }
}

public final class VoiceProcessingEngine:
    SidecarAudioService,
    PCMPlaybackScheduling,
    @unchecked Sendable
{
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

    private var captureHandler: (@Sendable (AVAudioPCMBuffer) -> Void)?
    private var capturePump: CaptureBufferPump?
    private var failureHandler: FailureHandler?
    private var configurationObserver: NSObjectProtocol?
    private var sessionID: UInt64?
    private var inputSignature: FormatSignature?
    private var outputSignature: FormatSignature?
    private var running = false
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

    public func setCaptureHandler(
        _ handler: (@Sendable (AVAudioPCMBuffer) -> Void)?
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
            if !engine.attachedNodes.contains(player) {
                engine.attach(player)
            }
            let playbackFormat = AVAudioFormat(
                standardFormatWithSampleRate: outputFormat.sampleRate,
                channels: outputFormat.channelCount
            )!
            engine.connect(
                player,
                to: engine.mainMixerNode,
                format: playbackFormat
            )
            let ring = try CapturePCMBufferRing(
                format: inputFormat,
                frameCapacity: 4_096,
                capacity: 8
            )
            let pump = CaptureBufferPump(
                ring: ring
            ) { [weak self] buffer in
                self?.deliverCapture(buffer)
            }
            capturePump = pump
            inputNode.installTap(
                onBus: 0,
                bufferSize: 4_096,
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
            }
            installConfigurationObserver()
        } catch {
            inputNode.removeTap(onBus: 0)
            capturePump?.stop()
            capturePump = nil
            player.stop()
            engine.stop()
            throw SidecarServiceFailure(
                stage: .audioCapture,
                code: .audioDeviceUnavailable
            )
        }
    }

    public func stop() async {
        let (observer, pump) = stateLock.withLock {
            let value = configurationObserver
            configurationObserver = nil
            running = false
            sessionID = nil
            inputSignature = nil
            outputSignature = nil
            captureHandler = nil
            let pump = capturePump
            capturePump = nil
            return (value, pump)
        }
        if let observer {
            NotificationCenter.default.removeObserver(observer)
        }
        engine.inputNode.removeTap(onBus: 0)
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

    private func deliverCapture(_ buffer: AVAudioPCMBuffer) {
        let handler = stateLock.withLock {
            captureHandler
        }
        handler?(buffer)
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
        let fatal: (UInt64, FailureHandler?, CaptureBufferPump?)? =
            stateLock.withLock {
                guard running, let sessionID else {
                    return nil
                }
                running = false
                let pump = capturePump
                capturePump = nil
                return (sessionID, failureHandler, pump)
            }
        guard let (sessionID, handler, pump) = fatal else {
            return
        }
        engine.inputNode.removeTap(onBus: 0)
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
