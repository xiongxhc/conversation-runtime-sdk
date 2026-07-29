import Foundation

public enum VoiceContractError: Error, Equatable, Sendable {
    case invalidSampleRate
    case invalidChannelCount
    case emptyPCM
    case pcmPayloadTooLarge
    case unalignedPCM
}

public enum PCMSampleFormat: UInt16, Equatable, Sendable {
    case signed16LittleEndian = 1
    case float32LittleEndian = 2

    public var bytesPerSample: Int {
        switch self {
        case .signed16LittleEndian:
            2
        case .float32LittleEndian:
            4
        }
    }
}

public struct PCMFormat: Equatable, Sendable {
    public let sampleRateHz: UInt32
    public let channels: UInt16
    public let sampleFormat: PCMSampleFormat

    public init(sampleRateHz: UInt32, channels: UInt16, sampleFormat: PCMSampleFormat) {
        self.sampleRateHz = sampleRateHz
        self.channels = channels
        self.sampleFormat = sampleFormat
    }

    public var frameAlignmentBytes: Int {
        Int(channels) * sampleFormat.bytesPerSample
    }
}

public struct PCMFrame: Equatable, Sendable {
    public static let maximumBytes = 65_536

    public let turnID: UInt64
    public let generationID: UInt64
    public let utteranceID: UInt64
    public let sequence: UInt64
    public let format: PCMFormat
    public let bytes: Data

    public init(
        turnID: UInt64,
        generationID: UInt64,
        utteranceID: UInt64,
        sequence: UInt64,
        format: PCMFormat,
        bytes: Data
    ) throws {
        guard format.sampleRateHz > 0 else {
            throw VoiceContractError.invalidSampleRate
        }
        guard format.channels > 0 else {
            throw VoiceContractError.invalidChannelCount
        }
        guard !bytes.isEmpty else {
            throw VoiceContractError.emptyPCM
        }
        guard bytes.count <= Self.maximumBytes else {
            throw VoiceContractError.pcmPayloadTooLarge
        }
        guard bytes.count.isMultiple(of: format.frameAlignmentBytes) else {
            throw VoiceContractError.unalignedPCM
        }

        self.turnID = turnID
        self.generationID = generationID
        self.utteranceID = utteranceID
        self.sequence = sequence
        self.format = format
        self.bytes = bytes
    }

    public var identity: PlaybackFrameIdentity {
        PlaybackFrameIdentity(
            turnID: turnID,
            generationID: generationID,
            utteranceID: utteranceID,
            sequence: sequence
        )
    }
}

public struct PlaybackFrameIdentity: Equatable, Hashable, Sendable {
    public let turnID: UInt64
    public let generationID: UInt64
    public let utteranceID: UInt64
    public let sequence: UInt64

    public init(
        turnID: UInt64,
        generationID: UInt64,
        utteranceID: UInt64,
        sequence: UInt64
    ) {
        self.turnID = turnID
        self.generationID = generationID
        self.utteranceID = utteranceID
        self.sequence = sequence
    }
}

public enum VoiceActivity: Equatable, Sendable {
    case speechStarted(atMilliseconds: UInt64)
    case speechContinued(atMilliseconds: UInt64)
    case speechEnded(atMilliseconds: UInt64)
}

public struct RecognitionHypothesis: Equatable, Sendable {
    public let segmentID: UInt64
    public let text: String
    public let engineFinal: Bool

    public init(segmentID: UInt64, text: String, engineFinal: Bool) {
        self.segmentID = segmentID
        self.text = text
        self.engineFinal = engineFinal
    }
}

public enum RuntimeStage: String, Codable, Equatable, Sendable {
    case runtime
    case privacyPolicy = "privacy_policy"
    case audioCapture = "audio_capture"
    case speechRecognizer = "speech_recognizer"
    case languageModel = "language_model"
    case speechSynthesizer = "speech_synthesizer"
    case audioOutput = "audio_output"
    case voiceSidecar = "voice_sidecar"
    case continuousAudioOutput = "continuous_audio_output"
}

public enum SidecarFailureCode: String, Codable, Equatable, Sendable {
    case permissionDenied = "permission_denied"
    case invalidState = "invalid_state"
    case malformedFrame = "malformed_frame"
    case audioDeviceUnavailable = "audio_device_unavailable"
    case recognitionFailed = "recognition_failed"
    case playbackFailed = "playback_failed"
    case `internal`
}
