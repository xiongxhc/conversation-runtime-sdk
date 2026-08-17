import Foundation

public enum ChildFrameKind: UInt16, CaseIterable, Equatable, Sendable {
    case startSession = 0x0001
    case startCapture = 0x0002
    case flushGeneration = 0x0003
    case shutdown = 0x0004
    case pauseCapture = 0x0005
    case resumeCapture = 0x0006
    case audioFrame = 0x0100
    case ready = 0x8001
    case voiceActivity = 0x8002
    case transcriptHypothesis = 0x8003
    case playbackAccepted = 0x8004
    case playbackRendered = 0x8005
    case playbackFlushed = 0x8006
    case captureStarted = 0x8007
    case capturePaused = 0x8008
    case captureResumed = 0x8009
    case audioDeviceStatus = 0x800A
    case failure = 0x80FE
    case shutdownComplete = 0x80FF

    var maximumPayloadBytes: Int {
        self == .audioFrame
            ? ChildProtocol.maximumAudioPayloadBytes
            : ChildProtocol.maximumControlPayloadBytes
    }
}

public enum ChildControl: Equatable, Sendable {
    case startSession(
        sessionID: UInt64,
        speechStartMilliseconds: UInt64,
        finalSilenceMilliseconds: UInt64
    )
    case startCapture(sessionID: UInt64, operationID: UInt64)
    case pauseCapture(sessionID: UInt64, operationID: UInt64)
    case resumeCapture(sessionID: UInt64, operationID: UInt64)
    case flushGeneration(sessionID: UInt64, generationID: UInt64, operationID: UInt64)
    case shutdown(sessionID: UInt64)
    case ready(sessionID: UInt64)
    case voiceActivity(sessionID: UInt64, activity: VoiceActivity)
    case transcriptHypothesis(sessionID: UInt64, hypothesis: RecognitionHypothesis)
    case playbackAccepted(
        sessionID: UInt64,
        turnID: UInt64,
        generationID: UInt64,
        utteranceID: UInt64,
        sequence: UInt64
    )
    case playbackRendered(
        sessionID: UInt64,
        turnID: UInt64,
        generationID: UInt64,
        utteranceID: UInt64,
        sequence: UInt64
    )
    case playbackFlushed(sessionID: UInt64, generationID: UInt64, operationID: UInt64)
    case captureStarted(sessionID: UInt64, operationID: UInt64)
    case capturePaused(sessionID: UInt64, operationID: UInt64)
    case captureResumed(sessionID: UInt64, operationID: UInt64)
    case audioDeviceStatus(
        sessionID: UInt64,
        inputLabel: String,
        outputLabel: String
    )
    case failure(sessionID: UInt64, stage: RuntimeStage, code: SidecarFailureCode)
    case shutdownComplete(sessionID: UInt64)

    public var kind: ChildFrameKind {
        switch self {
        case .startSession:
            .startSession
        case .startCapture:
            .startCapture
        case .pauseCapture:
            .pauseCapture
        case .resumeCapture:
            .resumeCapture
        case .flushGeneration:
            .flushGeneration
        case .shutdown:
            .shutdown
        case .ready:
            .ready
        case .voiceActivity:
            .voiceActivity
        case .transcriptHypothesis:
            .transcriptHypothesis
        case .playbackAccepted:
            .playbackAccepted
        case .playbackRendered:
            .playbackRendered
        case .playbackFlushed:
            .playbackFlushed
        case .captureStarted:
            .captureStarted
        case .capturePaused:
            .capturePaused
        case .captureResumed:
            .captureResumed
        case .audioDeviceStatus:
            .audioDeviceStatus
        case .failure:
            .failure
        case .shutdownComplete:
            .shutdownComplete
        }
    }

    public var sessionID: UInt64 {
        switch self {
        case let .startSession(sessionID, _, _),
             let .startCapture(sessionID, _),
             let .pauseCapture(sessionID, _),
             let .resumeCapture(sessionID, _),
             let .flushGeneration(sessionID, _, _),
             let .shutdown(sessionID),
             let .ready(sessionID),
             let .voiceActivity(sessionID, _),
             let .transcriptHypothesis(sessionID, _),
             let .playbackAccepted(sessionID, _, _, _, _),
             let .playbackRendered(sessionID, _, _, _, _),
             let .playbackFlushed(sessionID, _, _),
             let .captureStarted(sessionID, _),
             let .capturePaused(sessionID, _),
             let .captureResumed(sessionID, _),
             let .audioDeviceStatus(sessionID, _, _),
             let .failure(sessionID, _, _),
             let .shutdownComplete(sessionID):
            sessionID
        }
    }
}

public struct ChildAudio: Equatable, Sendable {
    public let sessionID: UInt64
    public let frame: PCMFrame

    public init(sessionID: UInt64, frame: PCMFrame) {
        self.sessionID = sessionID
        self.frame = frame
    }
}

public struct ChildFrame: Equatable, Sendable {
    public let version: UInt16
    public let control: ChildControl?
    public let audio: ChildAudio?

    public init(control: ChildControl) {
        version = ChildProtocol.version
        self.control = control
        audio = nil
    }

    public init(audioSessionID: UInt64, frame: PCMFrame) {
        version = ChildProtocol.version
        control = nil
        audio = ChildAudio(sessionID: audioSessionID, frame: frame)
    }

    init(version: UInt16, control: ChildControl) {
        self.version = version
        self.control = control
        audio = nil
    }

    init(version: UInt16, audio: ChildAudio) {
        self.version = version
        control = nil
        self.audio = audio
    }

    public var kind: ChildFrameKind {
        control?.kind ?? .audioFrame
    }
}

public enum ChildProtocolError: Error, Equatable, Sendable {
    case needMoreData(required: Int, available: Int)
    case truncatedFrame(required: Int, available: Int)
    case unknownVersion(UInt16)
    case unknownKind(UInt16)
    case payloadTooLarge(kind: ChildFrameKind, declared: Int, maximum: Int)
    case pcmPayloadTooLarge(declared: Int, maximum: Int)
    case payloadLengthOverflow
    case invalidControlUTF8
    case invalidControlJSON
    case invalidAudioMetadata(String)
    case unknownSampleFormat(UInt16)
    case trailingBytes(Int)
}

public enum ChildProtocol {
    public static let version: UInt16 = 1
    public static let headerBytes = 8
    public static let audioMetadataBytes = 48
    public static let maximumControlPayloadBytes = 65_536
    public static let maximumAudioPayloadBytes = audioMetadataBytes + PCMFrame.maximumBytes

    public static func encode(_ frame: ChildFrame) throws -> Data {
        let payload: Data
        if let control = frame.control {
            payload = try encodeControl(control)
        } else if let audio = frame.audio {
            payload = try encodeAudioPayload(audio)
        } else {
            throw ChildProtocolError.invalidControlJSON
        }

        try validatePayloadLength(kind: frame.kind, declared: payload.count)
        guard let payloadLength = UInt32(exactly: payload.count) else {
            throw ChildProtocolError.payloadLengthOverflow
        }

        var encoded = Data(capacity: headerBytes + payload.count)
        encoded.appendBigEndian(frame.version)
        encoded.appendBigEndian(frame.kind.rawValue)
        encoded.appendBigEndian(payloadLength)
        encoded.append(payload)
        return encoded
    }

    public static func decode(_ data: Data) throws -> ChildFrame {
        let header = try decodeHeader(data)
        let required = headerBytes + header.payloadLength
        guard data.count >= required else {
            throw ChildProtocolError.needMoreData(required: required, available: data.count)
        }
        guard data.count == required else {
            throw ChildProtocolError.trailingBytes(data.count - required)
        }

        let payload = data.subdata(in: headerBytes..<required)
        if header.kind == .audioFrame {
            return ChildFrame(version: header.version, audio: try decodeAudioPayload(payload))
        }
        return ChildFrame(
            version: header.version,
            control: try decodeControl(kind: header.kind, payload: payload)
        )
    }

    public static func decodeAtEOF(_ data: Data) throws -> ChildFrame {
        do {
            return try decode(data)
        } catch let error as ChildProtocolError {
            guard case let .needMoreData(required, available) = error else {
                throw error
            }
            throw ChildProtocolError.truncatedFrame(required: required, available: available)
        }
    }

    static func decodeHeader(_ data: Data) throws -> DecodedHeader {
        guard data.count >= headerBytes else {
            throw ChildProtocolError.needMoreData(required: headerBytes, available: data.count)
        }
        let bytes = [UInt8](data.prefix(headerBytes))
        let decodedVersion = readUInt16(bytes, at: 0)
        guard decodedVersion == version else {
            throw ChildProtocolError.unknownVersion(decodedVersion)
        }
        let kindCode = readUInt16(bytes, at: 2)
        guard let kind = ChildFrameKind(rawValue: kindCode) else {
            throw ChildProtocolError.unknownKind(kindCode)
        }
        let payloadLength = Int(readUInt32(bytes, at: 4))
        try validatePayloadLength(kind: kind, declared: payloadLength)
        return DecodedHeader(
            version: decodedVersion,
            kind: kind,
            payloadLength: payloadLength
        )
    }

    static func decodeAudioPayload(_ payload: Data) throws -> ChildAudio {
        let pcmLength = max(payload.count - audioMetadataBytes, 0)
        guard pcmLength <= PCMFrame.maximumBytes else {
            throw ChildProtocolError.pcmPayloadTooLarge(
                declared: pcmLength,
                maximum: PCMFrame.maximumBytes
            )
        }
        guard payload.count >= audioMetadataBytes else {
            throw ChildProtocolError.invalidAudioMetadata(
                "audio payload is shorter than 48-byte metadata"
            )
        }

        let bytes = [UInt8](payload)
        let sessionID = readUInt64(bytes, at: 0)
        let turnID = readUInt64(bytes, at: 8)
        let generationID = readUInt64(bytes, at: 16)
        let utteranceID = readUInt64(bytes, at: 24)
        let sequence = readUInt64(bytes, at: 32)
        let sampleRateHz = readUInt32(bytes, at: 40)
        let channels = readUInt16(bytes, at: 44)
        let sampleFormatCode = readUInt16(bytes, at: 46)

        guard let sampleFormat = PCMSampleFormat(rawValue: sampleFormatCode) else {
            throw ChildProtocolError.unknownSampleFormat(sampleFormatCode)
        }
        guard sampleRateHz > 0 else {
            throw ChildProtocolError.invalidAudioMetadata(
                "PCM sample rate must be greater than zero"
            )
        }
        guard channels > 0 else {
            throw ChildProtocolError.invalidAudioMetadata(
                "PCM channels must be greater than zero"
            )
        }

        let pcm = payload.subdata(in: audioMetadataBytes..<payload.count)
        guard !pcm.isEmpty else {
            throw ChildProtocolError.invalidAudioMetadata("PCM frame bytes must not be empty")
        }
        let format = PCMFormat(
            sampleRateHz: sampleRateHz,
            channels: channels,
            sampleFormat: sampleFormat
        )
        guard pcm.count.isMultiple(of: format.frameAlignmentBytes) else {
            throw ChildProtocolError.invalidAudioMetadata(
                "PCM frame bytes were not sample aligned"
            )
        }

        do {
            return ChildAudio(
                sessionID: sessionID,
                frame: try PCMFrame(
                    turnID: turnID,
                    generationID: generationID,
                    utteranceID: utteranceID,
                    sequence: sequence,
                    format: format,
                    bytes: pcm
                )
            )
        } catch {
            throw ChildProtocolError.invalidAudioMetadata("PCM frame validation failed")
        }
    }

    private static func validatePayloadLength(
        kind: ChildFrameKind,
        declared: Int
    ) throws {
        let maximum = kind.maximumPayloadBytes
        guard declared <= maximum else {
            throw ChildProtocolError.payloadTooLarge(
                kind: kind,
                declared: declared,
                maximum: maximum
            )
        }
    }

    private static func encodeAudioPayload(_ audio: ChildAudio) throws -> Data {
        let frame = audio.frame
        guard frame.bytes.count <= PCMFrame.maximumBytes else {
            throw ChildProtocolError.pcmPayloadTooLarge(
                declared: frame.bytes.count,
                maximum: PCMFrame.maximumBytes
            )
        }

        var payload = Data(capacity: audioMetadataBytes + frame.bytes.count)
        payload.appendBigEndian(audio.sessionID)
        payload.appendBigEndian(frame.turnID)
        payload.appendBigEndian(frame.generationID)
        payload.appendBigEndian(frame.utteranceID)
        payload.appendBigEndian(frame.sequence)
        payload.appendBigEndian(frame.format.sampleRateHz)
        payload.appendBigEndian(frame.format.channels)
        payload.appendBigEndian(frame.format.sampleFormat.rawValue)
        payload.append(frame.bytes)
        return payload
    }

    static func preflightControlPayloadLength(
        _ control: ChildControl
    ) throws -> Int {
        guard case let .transcriptHypothesis(
            sessionID,
            hypothesis
        ) = control else {
            return try encodeControl(control).count
        }
        return try transcriptPayloadLength(
            sessionID: sessionID,
            hypothesis: hypothesis
        )
    }

    private static func encodeControl(_ control: ChildControl) throws -> Data {
        switch control {
        case let .startSession(sessionID, speechStartMilliseconds, finalSilenceMilliseconds):
            return Data(
                """
                {"session_id":\(sessionID),"speech_start_ms":\(speechStartMilliseconds),"final_silence_ms":\(finalSilenceMilliseconds)}
                """.utf8
            )
        case let .shutdown(sessionID),
             let .ready(sessionID),
             let .shutdownComplete(sessionID):
            return Data(#"{"session_id":\#(sessionID)}"#.utf8)
        case let .startCapture(sessionID, operationID),
             let .pauseCapture(sessionID, operationID),
             let .resumeCapture(sessionID, operationID),
             let .captureStarted(sessionID, operationID),
             let .capturePaused(sessionID, operationID),
             let .captureResumed(sessionID, operationID):
            guard sessionID > 0, operationID > 0 else {
                throw ChildProtocolError.invalidControlJSON
            }
            return Data(
                #"{"session_id":\#(sessionID),"operation_id":\#(operationID)}"#.utf8
            )
        case let .flushGeneration(sessionID, generationID, operationID),
             let .playbackFlushed(sessionID, generationID, operationID):
            return Data(
                """
                {"session_id":\(sessionID),"generation_id":\(generationID),"operation_id":\(operationID)}
                """.utf8
            )
        case let .voiceActivity(sessionID, activity):
            switch activity {
            case let .speechStarted(atMilliseconds):
                return Data(
                    """
                    {"session_id":\(sessionID),"activity":"speech_started","at_ms":\(atMilliseconds)}
                    """.utf8
                )
            case let .speechContinued(atMilliseconds):
                return Data(
                    """
                    {"session_id":\(sessionID),"activity":"speech_continued","at_ms":\(atMilliseconds)}
                    """.utf8
                )
            case let .speechEnded(atMilliseconds):
                return Data(
                    """
                    {"session_id":\(sessionID),"activity":"speech_ended","at_ms":\(atMilliseconds)}
                    """.utf8
                )
            case let .captureDiscontinuity(atMilliseconds):
                return Data(
                    """
                    {"session_id":\(sessionID),"activity":"capture_discontinuity","at_ms":\(atMilliseconds)}
                    """.utf8
                )
            }
        case let .transcriptHypothesis(sessionID, hypothesis):
            return try encodeTranscriptHypothesis(
                sessionID: sessionID,
                hypothesis: hypothesis
            )
        case let .audioDeviceStatus(sessionID, inputLabel, outputLabel):
            guard sessionID > 0 else {
                throw ChildProtocolError.invalidControlJSON
            }
            var payload = Data()
            payload.append(contentsOf: #"{"session_id":"#.utf8)
            payload.append(contentsOf: String(sessionID).utf8)
            payload.append(contentsOf: #","input_label":"#.utf8)
            appendEscapedJSONString(inputLabel, to: &payload)
            payload.append(contentsOf: #","output_label":"#.utf8)
            appendEscapedJSONString(outputLabel, to: &payload)
            payload.append(0x7D)
            return payload
        case let .playbackAccepted(
            sessionID,
            turnID,
            generationID,
            utteranceID,
            sequence
        ),
             let .playbackRendered(
                 sessionID,
                 turnID,
                 generationID,
                 utteranceID,
                 sequence
             ):
            return Data(
                """
                {"session_id":\(sessionID),"turn_id":\(turnID),"generation_id":\(generationID),"utterance_id":\(utteranceID),"sequence":\(sequence)}
                """.utf8
            )
        case let .failure(sessionID, stage, code):
            return Data(
                """
                {"session_id":\(sessionID),"stage":"\(stage.rawValue)","code":"\(code.rawValue)"}
                """.utf8
            )
        }
    }

    private static func transcriptPayloadLength(
        sessionID: UInt64,
        hypothesis: RecognitionHypothesis
    ) throws -> Int {
        var length = 0
        try addByteCount(#"{"session_id":"#.utf8.count, to: &length)
        try addByteCount(String(sessionID).utf8.count, to: &length)
        try addByteCount(#","segment_id":"#.utf8.count, to: &length)
        try addByteCount(String(hypothesis.segmentID).utf8.count, to: &length)
        try addByteCount(#","text":"#.utf8.count, to: &length)
        try addByteCount(
            try escapedJSONStringByteCount(hypothesis.text),
            to: &length
        )
        try addByteCount(#","engine_final":"#.utf8.count, to: &length)
        try addByteCount(hypothesis.engineFinal ? 4 : 5, to: &length)
        try addByteCount(1, to: &length)
        guard length <= maximumControlPayloadBytes else {
            throw ChildProtocolError.payloadTooLarge(
                kind: .transcriptHypothesis,
                declared: length,
                maximum: maximumControlPayloadBytes
            )
        }
        return length
    }

    private static func encodeTranscriptHypothesis(
        sessionID: UInt64,
        hypothesis: RecognitionHypothesis
    ) throws -> Data {
        let length = try transcriptPayloadLength(
            sessionID: sessionID,
            hypothesis: hypothesis
        )
        var payload = Data(capacity: length)
        payload.append(contentsOf: #"{"session_id":"#.utf8)
        payload.append(contentsOf: String(sessionID).utf8)
        payload.append(contentsOf: #","segment_id":"#.utf8)
        payload.append(contentsOf: String(hypothesis.segmentID).utf8)
        payload.append(contentsOf: #","text":"#.utf8)
        appendEscapedJSONString(hypothesis.text, to: &payload)
        payload.append(contentsOf: #","engine_final":"#.utf8)
        payload.append(
            contentsOf: hypothesis.engineFinal ? "true".utf8 : "false".utf8
        )
        payload.append(0x7D)
        return payload
    }

    private static func escapedJSONStringByteCount(
        _ value: String
    ) throws -> Int {
        var length = 2
        for byte in value.utf8 {
            let increment: Int
            switch byte {
            case 0x08, 0x09, 0x0A, 0x0C, 0x0D, 0x22, 0x5C:
                increment = 2
            case 0x00...0x1F:
                increment = 6
            default:
                increment = 1
            }
            try addByteCount(increment, to: &length)
        }
        return length
    }

    private static func addByteCount(
        _ count: Int,
        to total: inout Int
    ) throws {
        let (next, overflow) = total.addingReportingOverflow(count)
        guard !overflow else {
            throw ChildProtocolError.payloadLengthOverflow
        }
        total = next
    }

    private static func appendEscapedJSONString(
        _ value: String,
        to data: inout Data
    ) {
        data.append(0x22)
        for byte in value.utf8 {
            switch byte {
            case 0x08:
                data.append(contentsOf: [0x5C, 0x62])
            case 0x09:
                data.append(contentsOf: [0x5C, 0x74])
            case 0x0A:
                data.append(contentsOf: [0x5C, 0x6E])
            case 0x0C:
                data.append(contentsOf: [0x5C, 0x66])
            case 0x0D:
                data.append(contentsOf: [0x5C, 0x72])
            case 0x22:
                data.append(contentsOf: [0x5C, 0x22])
            case 0x5C:
                data.append(contentsOf: [0x5C, 0x5C])
            case 0x00...0x1F:
                data.append(contentsOf: [0x5C, 0x75, 0x30, 0x30])
                data.append(hexDigit(byte >> 4))
                data.append(hexDigit(byte & 0x0F))
            default:
                data.append(byte)
            }
        }
        data.append(0x22)
    }

    private static func hexDigit(_ value: UInt8) -> UInt8 {
        value < 10 ? 0x30 + value : 0x61 + value - 10
    }

    private static func decodeControl(
        kind: ChildFrameKind,
        payload: Data
    ) throws -> ChildControl {
        guard String(data: payload, encoding: .utf8) != nil else {
            throw ChildProtocolError.invalidControlUTF8
        }

        do {
            switch kind {
            case .startSession:
                let value: StartSessionDTO = try decodeStrict(
                    payload,
                    keys: ["session_id", "speech_start_ms", "final_silence_ms"],
                    integerKeys: ["session_id", "speech_start_ms", "final_silence_ms"]
                )
                return .startSession(
                    sessionID: value.sessionID,
                    speechStartMilliseconds: value.speechStartMilliseconds,
                    finalSilenceMilliseconds: value.finalSilenceMilliseconds
                )
            case .shutdown, .ready, .shutdownComplete:
                let value: SessionDTO = try decodeStrict(
                    payload,
                    keys: ["session_id"],
                    integerKeys: ["session_id"]
                )
                switch kind {
                case .shutdown:
                    return .shutdown(sessionID: value.sessionID)
                case .ready:
                    return .ready(sessionID: value.sessionID)
                case .shutdownComplete:
                    return .shutdownComplete(sessionID: value.sessionID)
                default:
                    throw ChildProtocolError.invalidControlJSON
                }
            case .startCapture,
                 .pauseCapture,
                 .resumeCapture,
                 .captureStarted,
                 .capturePaused,
                 .captureResumed:
                let value: CaptureIdentityDTO = try decodeStrict(
                    payload,
                    keys: ["session_id", "operation_id"],
                    integerKeys: ["session_id", "operation_id"]
                )
                guard value.sessionID > 0, value.operationID > 0 else {
                    throw ChildProtocolError.invalidControlJSON
                }
                switch kind {
                case .startCapture:
                    return .startCapture(
                        sessionID: value.sessionID,
                        operationID: value.operationID
                    )
                case .pauseCapture:
                    return .pauseCapture(
                        sessionID: value.sessionID,
                        operationID: value.operationID
                    )
                case .resumeCapture:
                    return .resumeCapture(
                        sessionID: value.sessionID,
                        operationID: value.operationID
                    )
                case .captureStarted:
                    return .captureStarted(
                        sessionID: value.sessionID,
                        operationID: value.operationID
                    )
                case .capturePaused:
                    return .capturePaused(
                        sessionID: value.sessionID,
                        operationID: value.operationID
                    )
                case .captureResumed:
                    return .captureResumed(
                        sessionID: value.sessionID,
                        operationID: value.operationID
                    )
                default:
                    throw ChildProtocolError.invalidControlJSON
                }
            case .flushGeneration, .playbackFlushed:
                let value: FlushIdentityDTO = try decodeStrict(
                    payload,
                    keys: ["session_id", "generation_id", "operation_id"],
                    integerKeys: ["session_id", "generation_id", "operation_id"]
                )
                if kind == .flushGeneration {
                    return .flushGeneration(
                        sessionID: value.sessionID,
                        generationID: value.generationID,
                        operationID: value.operationID
                    )
                }
                return .playbackFlushed(
                    sessionID: value.sessionID,
                    generationID: value.generationID,
                    operationID: value.operationID
                )
            case .voiceActivity:
                let value: VoiceActivityDTO = try decodeStrict(
                    payload,
                    keys: ["session_id", "activity", "at_ms"],
                    integerKeys: ["session_id", "at_ms"]
                )
                let activity: VoiceActivity
                switch value.activity {
                case .started:
                    activity = .speechStarted(atMilliseconds: value.atMilliseconds)
                case .continued:
                    activity = .speechContinued(atMilliseconds: value.atMilliseconds)
                case .ended:
                    activity = .speechEnded(atMilliseconds: value.atMilliseconds)
                case .discontinuity:
                    activity = .captureDiscontinuity(
                        atMilliseconds: value.atMilliseconds
                    )
                }
                return .voiceActivity(sessionID: value.sessionID, activity: activity)
            case .transcriptHypothesis:
                let value: TranscriptHypothesisDTO = try decodeStrict(
                    payload,
                    keys: ["session_id", "segment_id", "text", "engine_final"],
                    integerKeys: ["session_id", "segment_id"]
                )
                return .transcriptHypothesis(
                    sessionID: value.sessionID,
                    hypothesis: RecognitionHypothesis(
                        segmentID: value.segmentID,
                        text: value.text,
                        engineFinal: value.engineFinal
                    )
                )
            case .audioDeviceStatus:
                let value: AudioDeviceStatusDTO = try decodeStrict(
                    payload,
                    keys: ["session_id", "input_label", "output_label"],
                    integerKeys: ["session_id"]
                )
                guard value.sessionID > 0 else {
                    throw ChildProtocolError.invalidControlJSON
                }
                return .audioDeviceStatus(
                    sessionID: value.sessionID,
                    inputLabel: value.inputLabel,
                    outputLabel: value.outputLabel
                )
            case .playbackAccepted, .playbackRendered:
                let value: MediaIdentityDTO = try decodeStrict(
                    payload,
                    keys: [
                        "session_id",
                        "turn_id",
                        "generation_id",
                        "utterance_id",
                        "sequence",
                    ],
                    integerKeys: [
                        "session_id",
                        "turn_id",
                        "generation_id",
                        "utterance_id",
                        "sequence",
                    ]
                )
                if kind == .playbackAccepted {
                    return .playbackAccepted(
                        sessionID: value.sessionID,
                        turnID: value.turnID,
                        generationID: value.generationID,
                        utteranceID: value.utteranceID,
                        sequence: value.sequence
                    )
                }
                return .playbackRendered(
                    sessionID: value.sessionID,
                    turnID: value.turnID,
                    generationID: value.generationID,
                    utteranceID: value.utteranceID,
                    sequence: value.sequence
                )
            case .failure:
                let value: FailureDTO = try decodeStrict(
                    payload,
                    keys: ["session_id", "stage", "code"],
                    integerKeys: ["session_id"]
                )
                return .failure(
                    sessionID: value.sessionID,
                    stage: value.stage,
                    code: value.code
                )
            case .audioFrame:
                throw ChildProtocolError.invalidControlJSON
            }
        } catch is ChildProtocolError {
            throw ChildProtocolError.invalidControlJSON
        } catch {
            throw ChildProtocolError.invalidControlJSON
        }
    }

    private static func decodeStrict<Value: Decodable>(
        _ payload: Data,
        keys expectedKeys: Set<String>,
        integerKeys: Set<String>
    ) throws -> Value {
        let bytes = [UInt8](payload)
        let fields = try topLevelFields(in: bytes)
        guard Set(fields.keys) == expectedKeys else {
            throw ChildProtocolError.invalidControlJSON
        }
        for key in integerKeys {
            guard let range = fields[key],
                  isCanonicalUnsignedInteger(bytes[range])
            else {
                throw ChildProtocolError.invalidControlJSON
            }
        }
        return try JSONDecoder().decode(Value.self, from: payload)
    }

    private static func topLevelFields(
        in bytes: [UInt8]
    ) throws -> [String: Range<Int>] {
        var index = 0
        skipWhitespace(bytes, index: &index)
        guard take(0x7B, from: bytes, index: &index) else {
            throw ChildProtocolError.invalidControlJSON
        }

        var fields: [String: Range<Int>] = [:]
        skipWhitespace(bytes, index: &index)
        if take(0x7D, from: bytes, index: &index) {
            skipWhitespace(bytes, index: &index)
            guard index == bytes.count else {
                throw ChildProtocolError.invalidControlJSON
            }
            return fields
        }

        while index < bytes.count {
            let keyStart = index
            try scanJSONString(bytes, index: &index)
            let keyData = Data(bytes[keyStart..<index])
            let key = try JSONDecoder().decode(String.self, from: keyData)
            guard fields[key] == nil else {
                throw ChildProtocolError.invalidControlJSON
            }

            skipWhitespace(bytes, index: &index)
            guard take(0x3A, from: bytes, index: &index) else {
                throw ChildProtocolError.invalidControlJSON
            }
            skipWhitespace(bytes, index: &index)
            let valueStart = index
            try skipJSONValue(bytes, index: &index)
            fields[key] = valueStart..<index
            skipWhitespace(bytes, index: &index)

            if take(0x2C, from: bytes, index: &index) {
                skipWhitespace(bytes, index: &index)
                continue
            }
            guard take(0x7D, from: bytes, index: &index) else {
                throw ChildProtocolError.invalidControlJSON
            }
            skipWhitespace(bytes, index: &index)
            guard index == bytes.count else {
                throw ChildProtocolError.invalidControlJSON
            }
            return fields
        }

        throw ChildProtocolError.invalidControlJSON
    }

    private static func isCanonicalUnsignedInteger(
        _ token: ArraySlice<UInt8>
    ) -> Bool {
        var start = token.startIndex
        var end = token.endIndex
        while start < end, isJSONWhitespace(token[start]) {
            start += 1
        }
        while end > start, isJSONWhitespace(token[token.index(before: end)]) {
            end = token.index(before: end)
        }
        guard start < end else {
            return false
        }
        if token[start] == 0x30 {
            return token.index(after: start) == end
        }
        guard (0x31...0x39).contains(token[start]) else {
            return false
        }
        var index = token.index(after: start)
        while index < end {
            guard (0x30...0x39).contains(token[index]) else {
                return false
            }
            index = token.index(after: index)
        }
        return true
    }

    private static func isJSONWhitespace(_ byte: UInt8) -> Bool {
        byte == 0x20 || byte == 0x09 || byte == 0x0A || byte == 0x0D
    }

    private static func scanJSONString(
        _ bytes: [UInt8],
        index: inout Int
    ) throws {
        guard take(0x22, from: bytes, index: &index) else {
            throw ChildProtocolError.invalidControlJSON
        }
        while index < bytes.count {
            let byte = bytes[index]
            index += 1
            if byte == 0x22 {
                return
            }
            if byte == 0x5C {
                guard index < bytes.count else {
                    throw ChildProtocolError.invalidControlJSON
                }
                index += 1
            }
        }
        throw ChildProtocolError.invalidControlJSON
    }

    private static func skipJSONValue(
        _ bytes: [UInt8],
        index: inout Int
    ) throws {
        let start = index
        var objectDepth = 0
        var arrayDepth = 0
        var inString = false
        var escaped = false

        while index < bytes.count {
            let byte = bytes[index]
            if inString {
                index += 1
                if escaped {
                    escaped = false
                } else if byte == 0x5C {
                    escaped = true
                } else if byte == 0x22 {
                    inString = false
                }
                continue
            }

            switch byte {
            case 0x22:
                inString = true
                index += 1
            case 0x7B:
                objectDepth += 1
                index += 1
            case 0x7D where objectDepth > 0:
                objectDepth -= 1
                index += 1
            case 0x5B:
                arrayDepth += 1
                index += 1
            case 0x5D where arrayDepth > 0:
                arrayDepth -= 1
                index += 1
            case 0x2C where objectDepth == 0 && arrayDepth == 0,
                 0x7D where objectDepth == 0 && arrayDepth == 0:
                guard index > start else {
                    throw ChildProtocolError.invalidControlJSON
                }
                return
            default:
                index += 1
            }
        }

        throw ChildProtocolError.invalidControlJSON
    }

    private static func skipWhitespace(_ bytes: [UInt8], index: inout Int) {
        while index < bytes.count,
              isJSONWhitespace(bytes[index])
        {
            index += 1
        }
    }

    private static func take(
        _ byte: UInt8,
        from bytes: [UInt8],
        index: inout Int
    ) -> Bool {
        guard index < bytes.count, bytes[index] == byte else {
            return false
        }
        index += 1
        return true
    }
}

struct DecodedHeader: Equatable, Sendable {
    let version: UInt16
    let kind: ChildFrameKind
    let payloadLength: Int
}

private struct StartSessionDTO: Decodable {
    let sessionID: UInt64
    let speechStartMilliseconds: UInt64
    let finalSilenceMilliseconds: UInt64

    enum CodingKeys: String, CodingKey {
        case sessionID = "session_id"
        case speechStartMilliseconds = "speech_start_ms"
        case finalSilenceMilliseconds = "final_silence_ms"
    }
}

private struct SessionDTO: Decodable {
    let sessionID: UInt64

    enum CodingKeys: String, CodingKey {
        case sessionID = "session_id"
    }
}

private struct CaptureIdentityDTO: Decodable {
    let sessionID: UInt64
    let operationID: UInt64

    enum CodingKeys: String, CodingKey {
        case sessionID = "session_id"
        case operationID = "operation_id"
    }
}

private struct FlushIdentityDTO: Decodable {
    let sessionID: UInt64
    let generationID: UInt64
    let operationID: UInt64

    enum CodingKeys: String, CodingKey {
        case sessionID = "session_id"
        case generationID = "generation_id"
        case operationID = "operation_id"
    }
}

private struct MediaIdentityDTO: Decodable {
    let sessionID: UInt64
    let turnID: UInt64
    let generationID: UInt64
    let utteranceID: UInt64
    let sequence: UInt64

    enum CodingKeys: String, CodingKey {
        case sessionID = "session_id"
        case turnID = "turn_id"
        case generationID = "generation_id"
        case utteranceID = "utterance_id"
        case sequence
    }
}

private enum VoiceActivityKindDTO: String, Decodable {
    case started = "speech_started"
    case continued = "speech_continued"
    case ended = "speech_ended"
    case discontinuity = "capture_discontinuity"
}

private struct VoiceActivityDTO: Decodable {
    let sessionID: UInt64
    let activity: VoiceActivityKindDTO
    let atMilliseconds: UInt64

    enum CodingKeys: String, CodingKey {
        case sessionID = "session_id"
        case activity
        case atMilliseconds = "at_ms"
    }
}

private struct TranscriptHypothesisDTO: Decodable {
    let sessionID: UInt64
    let segmentID: UInt64
    let text: String
    let engineFinal: Bool

    enum CodingKeys: String, CodingKey {
        case sessionID = "session_id"
        case segmentID = "segment_id"
        case text
        case engineFinal = "engine_final"
    }
}

private struct AudioDeviceStatusDTO: Decodable {
    let sessionID: UInt64
    let inputLabel: String
    let outputLabel: String

    enum CodingKeys: String, CodingKey {
        case sessionID = "session_id"
        case inputLabel = "input_label"
        case outputLabel = "output_label"
    }
}

private struct FailureDTO: Decodable {
    let sessionID: UInt64
    let stage: RuntimeStage
    let code: SidecarFailureCode

    enum CodingKeys: String, CodingKey {
        case sessionID = "session_id"
        case stage
        case code
    }
}

private func readUInt16(_ bytes: [UInt8], at offset: Int) -> UInt16 {
    UInt16(bytes[offset]) << 8
        | UInt16(bytes[offset + 1])
}

private func readUInt32(_ bytes: [UInt8], at offset: Int) -> UInt32 {
    UInt32(bytes[offset]) << 24
        | UInt32(bytes[offset + 1]) << 16
        | UInt32(bytes[offset + 2]) << 8
        | UInt32(bytes[offset + 3])
}

private func readUInt64(_ bytes: [UInt8], at offset: Int) -> UInt64 {
    UInt64(bytes[offset]) << 56
        | UInt64(bytes[offset + 1]) << 48
        | UInt64(bytes[offset + 2]) << 40
        | UInt64(bytes[offset + 3]) << 32
        | UInt64(bytes[offset + 4]) << 24
        | UInt64(bytes[offset + 5]) << 16
        | UInt64(bytes[offset + 6]) << 8
        | UInt64(bytes[offset + 7])
}

private extension Data {
    mutating func appendBigEndian(_ value: UInt16) {
        append(UInt8(truncatingIfNeeded: value >> 8))
        append(UInt8(truncatingIfNeeded: value))
    }

    mutating func appendBigEndian(_ value: UInt32) {
        append(UInt8(truncatingIfNeeded: value >> 24))
        append(UInt8(truncatingIfNeeded: value >> 16))
        append(UInt8(truncatingIfNeeded: value >> 8))
        append(UInt8(truncatingIfNeeded: value))
    }

    mutating func appendBigEndian(_ value: UInt64) {
        append(UInt8(truncatingIfNeeded: value >> 56))
        append(UInt8(truncatingIfNeeded: value >> 48))
        append(UInt8(truncatingIfNeeded: value >> 40))
        append(UInt8(truncatingIfNeeded: value >> 32))
        append(UInt8(truncatingIfNeeded: value >> 24))
        append(UInt8(truncatingIfNeeded: value >> 16))
        append(UInt8(truncatingIfNeeded: value >> 8))
        append(UInt8(truncatingIfNeeded: value))
    }
}
