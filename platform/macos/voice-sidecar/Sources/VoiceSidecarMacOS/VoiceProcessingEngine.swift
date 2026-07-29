@preconcurrency import AVFoundation
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
    private let captureContinuation: AsyncStream<AVAudioPCMBuffer>.Continuation
    private var captureTask: Task<Void, Never>?

    private var captureHandler: (@Sendable (AVAudioPCMBuffer) -> Void)?
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
        let (stream, continuation) = AsyncStream<AVAudioPCMBuffer>.makeStream(
            bufferingPolicy: .bufferingNewest(8)
        )
        captureContinuation = continuation
        captureTask = Task.detached { [weak self] in
            for await buffer in stream {
                guard !Task.isCancelled else {
                    return
                }
                self?.deliverCapture(buffer)
            }
        }
    }

    deinit {
        captureContinuation.finish()
        captureTask?.cancel()
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
            inputNode.installTap(
                onBus: 0,
                bufferSize: 4_096,
                format: inputFormat
            ) { [weak self] buffer, _ in
                guard let copy = Self.copy(buffer) else {
                    return
                }
                self?.captureContinuation.yield(copy)
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
            player.stop()
            engine.stop()
            throw SidecarServiceFailure(
                stage: .audioCapture,
                code: .audioDeviceUnavailable
            )
        }
    }

    public func stop() async {
        let observer = stateLock.withLock {
            let value = configurationObserver
            configurationObserver = nil
            running = false
            sessionID = nil
            inputSignature = nil
            outputSignature = nil
            captureHandler = nil
            return value
        }
        if let observer {
            NotificationCenter.default.removeObserver(observer)
        }
        engine.inputNode.removeTap(onBus: 0)
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

    private static func copy(
        _ buffer: AVAudioPCMBuffer
    ) -> AVAudioPCMBuffer? {
        guard
            let copy = AVAudioPCMBuffer(
                pcmFormat: buffer.format,
                frameCapacity: buffer.frameLength
            )
        else {
            return nil
        }
        copy.frameLength = buffer.frameLength
        let source = UnsafeMutableAudioBufferListPointer(
            buffer.mutableAudioBufferList
        )
        let destination = UnsafeMutableAudioBufferListPointer(
            copy.mutableAudioBufferList
        )
        guard source.count == destination.count else {
            return nil
        }
        for index in source.indices {
            guard let sourceData = source[index].mData,
                let destinationData = destination[index].mData
            else {
                return nil
            }
            memcpy(
                destinationData,
                sourceData,
                Int(source[index].mDataByteSize)
            )
            destination[index].mDataByteSize = source[index].mDataByteSize
        }
        return copy
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
        let fatal: (UInt64, FailureHandler?)? = stateLock.withLock {
            guard running, let sessionID else {
                return nil
            }
            running = false
            return (sessionID, failureHandler)
        }
        guard let (sessionID, handler) = fatal else {
            return
        }
        engine.inputNode.removeTap(onBus: 0)
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
