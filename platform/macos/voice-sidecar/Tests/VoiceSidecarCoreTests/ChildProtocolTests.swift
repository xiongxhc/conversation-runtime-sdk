import Foundation
import Testing
@testable import VoiceSidecarCore

@Test
func versionTwoCaptureKindCodesArePinned() {
    #expect(ChildProtocol.version == 2)
    let expected: [(ChildFrameKind, UInt16)] = [
        (.startSession, 0x0001),
        (.startCapture, 0x0002),
        (.flushGeneration, 0x0003),
        (.shutdown, 0x0004),
        (.pauseCapture, 0x0005),
        (.resumeCapture, 0x0006),
        (.audioFrame, 0x0100),
        (.ready, 0x8001),
        (.voiceActivity, 0x8002),
        (.transcriptHypothesis, 0x8003),
        (.playbackAccepted, 0x8004),
        (.playbackRendered, 0x8005),
        (.playbackFlushed, 0x8006),
        (.captureStarted, 0x8007),
        (.capturePaused, 0x8008),
        (.captureResumed, 0x8009),
        (.failure, 0x80FE),
        (.shutdownComplete, 0x80FF),
    ]

    for (kind, code) in expected {
        #expect(kind.rawValue == code)
    }
}

@Test
func captureControlsRoundTripExactSessionAndOperationIdentity() throws {
    let controls: [ChildControl] = [
        .startCapture(sessionID: 7, operationID: 1),
        .pauseCapture(sessionID: 7, operationID: 2),
        .resumeCapture(sessionID: 7, operationID: 3),
        .captureStarted(sessionID: 7, operationID: 1),
        .capturePaused(sessionID: 7, operationID: 2),
        .captureResumed(sessionID: 7, operationID: 3),
    ]

    for control in controls {
        let frame = ChildFrame(control: control)
        #expect(try ChildProtocol.decode(ChildProtocol.encode(frame)) == frame)
    }
}

@Test
func protocolV1IsRejectedExplicitly() {
    let data = rawFrame(
        version: 1,
        kind: .startCapture,
        payload: Data(#"{"session_id":7,"operation_id":1}"#.utf8)
    )

    #expect(throws: ChildProtocolError.unknownVersion(1)) {
        try ChildProtocol.decode(data)
    }
}

@Test
func captureControlsRejectZeroOperationIdentity() {
    #expect(throws: ChildProtocolError.invalidControlJSON) {
        try ChildProtocol.encode(
            ChildFrame(control: .pauseCapture(sessionID: 7, operationID: 0))
        )
    }
    #expect(throws: ChildProtocolError.invalidControlJSON) {
        try ChildProtocol.decode(
            rawFrame(
                kind: .capturePaused,
                payload: Data(#"{"session_id":7,"operation_id":0}"#.utf8)
            )
        )
    }
}

@Test
func captureControlsRejectZeroSessionIdentity() {
    let controls: [ChildControl] = [
        .startCapture(sessionID: 0, operationID: 1),
        .pauseCapture(sessionID: 0, operationID: 1),
        .resumeCapture(sessionID: 0, operationID: 1),
        .captureStarted(sessionID: 0, operationID: 1),
        .capturePaused(sessionID: 0, operationID: 1),
        .captureResumed(sessionID: 0, operationID: 1),
    ]
    for control in controls {
        #expect(throws: ChildProtocolError.invalidControlJSON) {
            try ChildProtocol.encode(ChildFrame(control: control))
        }
    }

    let kinds: [ChildFrameKind] = [
        .startCapture,
        .pauseCapture,
        .resumeCapture,
        .captureStarted,
        .capturePaused,
        .captureResumed,
    ]
    for kind in kinds {
        #expect(throws: ChildProtocolError.invalidControlJSON) {
            try ChildProtocol.decode(
                rawFrame(
                    kind: kind,
                    payload: Data(#"{"session_id":0,"operation_id":1}"#.utf8)
                )
            )
        }
    }
}

@Test
func startSessionFixtureRoundTrips() throws {
    let data = try Data(contentsOf: fixture("control/start-session.bin"))
    let frame = try ChildProtocol.decode(data)

    #expect(frame.version == 2)
    #expect(frame.kind == .startSession)
    #expect(try ChildProtocol.encode(frame) == data)
    #expect(
        data.dropFirst(ChildProtocol.headerBytes)
            == Data(#"{"session_id":7,"speech_start_ms":200,"final_silence_ms":600}"#.utf8)
    )
}

@Test
func transcriptFixtureRoundTrips() throws {
    let data = try Data(contentsOf: fixture("control/transcript-partial.bin"))
    let frame = try ChildProtocol.decode(data)

    #expect(frame.kind == .transcriptHypothesis)
    #expect(
        frame.control
            == .transcriptHypothesis(
                sessionID: 7,
                hypothesis: RecognitionHypothesis(segmentID: 3, text: "hel", engineFinal: false)
            )
    )
    #expect(try ChildProtocol.encode(frame) == data)
}

@Test
func signedSixteenAudioFixturePinsMetadataAndPCM() throws {
    let source = ChildFrame(
        audioSessionID: 1,
        frame: try PCMFrame(
            turnID: 2,
            generationID: 3,
            utteranceID: 4,
            sequence: 5,
            format: PCMFormat(
                sampleRateHz: 24_000,
                channels: 1,
                sampleFormat: .signed16LittleEndian
            ),
            bytes: Data([0x00, 0x80, 0xFF, 0x7F])
        )
    )
    let data = try ChildProtocol.encode(source)
    let frame = try ChildProtocol.decode(data)
    let audio = try #require(frame.audio)

    #expect(frame.kind == .audioFrame)
    #expect(audio.sessionID == 1)
    #expect(audio.frame.turnID == 2)
    #expect(audio.frame.generationID == 3)
    #expect(audio.frame.utteranceID == 4)
    #expect(audio.frame.sequence == 5)
    #expect(audio.frame.format == PCMFormat(sampleRateHz: 24_000, channels: 1, sampleFormat: .signed16LittleEndian))
    #expect(audio.frame.bytes == Data([0x00, 0x80, 0xFF, 0x7F]))
    #expect(try ChildProtocol.encode(frame) == data)
}

@Test
func floatThirtyTwoAudioUsesSampleFormatCodeTwo() throws {
    let frame = ChildFrame(
        audioSessionID: 1,
        frame: try PCMFrame(
            turnID: 2,
            generationID: 3,
            utteranceID: 4,
            sequence: 5,
            format: PCMFormat(
                sampleRateHz: 48_000,
                channels: 2,
                sampleFormat: .float32LittleEndian
            ),
            bytes: Data(repeating: 0, count: 8)
        )
    )
    let encoded = try ChildProtocol.encode(frame)

    #expect(
        encoded.subdata(
            in: (ChildProtocol.headerBytes + 46)..<(ChildProtocol.headerBytes + 48)
        ) == Data([0, 2])
    )
    #expect(try ChildProtocol.decode(encoded) == frame)
}

@Test
func everyPartialHeaderNeedsExactlyEightBytes() {
    let complete = header(kind: .ready, payloadLength: 0)

    for available in 0..<ChildProtocol.headerBytes {
        #expect(throws: ChildProtocolError.needMoreData(required: 8, available: available)) {
            try ChildProtocol.decode(complete.prefix(available))
        }
    }
}

@Test
func partialPayloadNeedsTheDeclaredLength() {
    let complete = rawFrame(kind: .startCapture, payload: Data(#"{"session_id":7}"#.utf8))

    for available in ChildProtocol.headerBytes..<complete.count {
        #expect(
            throws: ChildProtocolError.needMoreData(
                required: complete.count,
                available: available
            )
        ) {
            try ChildProtocol.decode(complete.prefix(available))
        }
    }
}

@Test
func eofConvertsPartialDataToTypedTruncation() throws {
    let complete = try ChildProtocol.encode(
        ChildFrame(
            control: .startSession(
                sessionID: 7,
                speechStartMilliseconds: 200,
                finalSilenceMilliseconds: 600
            )
        )
    )
    let data = complete.dropLast(3)

    do {
        _ = try ChildProtocol.decodeAtEOF(data)
        Issue.record("expected truncated frame")
    } catch let error as ChildProtocolError {
        guard case let .truncatedFrame(required, available) = error else {
            Issue.record("unexpected error \(error)")
            return
        }
        #expect(required == 69)
        #expect(available == data.count)
    }
}

@Test
func unknownVersionAndKindFailBeforePayloadDecode() {
    #expect(throws: ChildProtocolError.unknownVersion(1)) {
        try ChildProtocol.decode(rawFrame(version: 1, kindCode: 0x0002, payload: Data([0xFF])))
    }
    #expect(throws: ChildProtocolError.unknownKind(0x7777)) {
        try ChildProtocol.decode(rawFrame(kindCode: 0x7777, payload: Data([0xFF])))
    }
}

@Test
func malformedControlPayloadsAreTypedAndStrict() {
    #expect(throws: ChildProtocolError.invalidControlUTF8) {
        try ChildProtocol.decode(rawFrame(kind: .startCapture, payload: Data([0xFF])))
    }
    #expect(throws: ChildProtocolError.invalidControlJSON) {
        try ChildProtocol.decode(
            rawFrame(kind: .startCapture, payload: Data(#"{"session_id":"wrong"}"#.utf8))
        )
    }
    #expect(throws: ChildProtocolError.invalidControlJSON) {
        try ChildProtocol.decode(
            rawFrame(
                kind: .startCapture,
                payload: Data(#"{"session_id":7,"unexpected":true}"#.utf8)
            )
        )
    }
}

@Test
func duplicateControlFieldsAreRejectedLikeSerde() {
    #expect(throws: ChildProtocolError.invalidControlJSON) {
        try ChildProtocol.decode(
            rawFrame(
                kind: .startCapture,
                payload: Data(#"{"session_id":7,"session_id":8}"#.utf8)
            )
        )
    }
}

@Test
func everyUnsignedControlFieldRejectsNoncanonicalJSONNumbersLikeSerde() {
    let cases: [
        (kind: ChildFrameKind, payload: String, integerFields: [String])
    ] = [
        (
            .startSession,
            #"{"session_id":7,"speech_start_ms":7,"final_silence_ms":7}"#,
            ["session_id", "speech_start_ms", "final_silence_ms"]
        ),
        (
            .startCapture,
            #"{"session_id":7}"#,
            ["session_id"]
        ),
        (
            .flushGeneration,
            #"{"session_id":7,"generation_id":7,"operation_id":7}"#,
            ["session_id", "generation_id", "operation_id"]
        ),
        (
            .voiceActivity,
            #"{"session_id":7,"activity":"speech_started","at_ms":7}"#,
            ["session_id", "at_ms"]
        ),
        (
            .transcriptHypothesis,
            #"{"session_id":7,"segment_id":7,"text":"hello","engine_final":false}"#,
            ["session_id", "segment_id"]
        ),
        (
            .playbackAccepted,
            #"{"session_id":7,"turn_id":7,"generation_id":7,"utterance_id":7,"sequence":7}"#,
            ["session_id", "turn_id", "generation_id", "utterance_id", "sequence"]
        ),
        (
            .failure,
            #"{"session_id":7,"stage":"voice_sidecar","code":"invalid_state"}"#,
            ["session_id"]
        ),
    ]

    for item in cases {
        for field in item.integerFields {
            let canonical = #""\#(field)":7"#
            for noncanonical in ["7.0", "7e0", "2e2"] {
                let payload = item.payload.replacingOccurrences(
                    of: canonical,
                    with: #""\#(field)":\#(noncanonical)"#
                )
                #expect(payload != item.payload)
                #expect(throws: ChildProtocolError.invalidControlJSON) {
                    try ChildProtocol.decode(
                        rawFrame(kind: item.kind, payload: Data(payload.utf8))
                    )
                }
            }
        }
    }
}

@Test
func canonicalStringEscapingMatchesSerdeJSON() throws {
    let frame = ChildFrame(
        control: .transcriptHypothesis(
            sessionID: 1,
            hypothesis: RecognitionHypothesis(
                segmentID: 2,
                text: "quote\" slash\\ line\n tab\t control\u{0001} / 🙂",
                engineFinal: false
            )
        )
    )
    let encoded = try ChildProtocol.encode(frame)

    #expect(
        encoded.dropFirst(8)
            == Data(
                #"{"session_id":1,"segment_id":2,"text":"quote\" slash\\ line\n tab\t control\u0001 / 🙂","engine_final":false}"#.utf8
            )
    )
    #expect(try ChildProtocol.decode(encoded) == frame)
}

@Test
func transcriptEscapingIsPreflightedBeforeBoundedEncoding() throws {
    let exact = ChildControl.transcriptHypothesis(
        sessionID: 1,
        hypothesis: RecognitionHypothesis(
            segmentID: 1,
            text: String(repeating: "\"", count: 32_737),
            engineFinal: false
        )
    )
    let exactFrame = ChildFrame(control: exact)

    #expect(try ChildProtocol.preflightControlPayloadLength(exact) == 65_536)
    #expect(try ChildProtocol.encode(exactFrame).count == 8 + 65_536)

    let huge = ChildControl.transcriptHypothesis(
        sessionID: 1,
        hypothesis: RecognitionHypothesis(
            segmentID: 1,
            text: String(repeating: "\u{0001}", count: 1_000_000),
            engineFinal: false
        )
    )
    let expected = ChildProtocolError.payloadTooLarge(
        kind: .transcriptHypothesis,
        declared: 6_000_062,
        maximum: 65_536
    )

    #expect(throws: expected) {
        try ChildProtocol.preflightControlPayloadLength(huge)
    }
    #expect(throws: expected) {
        try ChildProtocol.encode(ChildFrame(control: huge))
    }
}

@Test
func oversizedDeclaredControlLengthFailsFromHeaderOnly() throws {
    let data = header(kind: .startSession, payloadLength: 65_537)

    #expect(
        throws: ChildProtocolError.payloadTooLarge(
            kind: .startSession,
            declared: 65_537,
            maximum: 65_536
        )
    ) {
        try ChildProtocol.decode(data)
    }
}

@Test
func maximumControlPayloadRoundTripsAndOneMoreFails() throws {
    let exact = ChildFrame(
        control: .transcriptHypothesis(
            sessionID: 1,
            hypothesis: RecognitionHypothesis(
                segmentID: 1,
                text: String(repeating: "x", count: 65_536 - 62),
                engineFinal: false
            )
        )
    )
    let encoded = try ChildProtocol.encode(exact)

    #expect(encoded.count == 8 + 65_536)
    #expect(try ChildProtocol.decode(encoded) == exact)

    let oversized = ChildFrame(
        control: .transcriptHypothesis(
            sessionID: 1,
            hypothesis: RecognitionHypothesis(
                segmentID: 1,
                text: String(repeating: "x", count: 65_536 - 61),
                engineFinal: false
            )
        )
    )
    #expect(
        throws: ChildProtocolError.payloadTooLarge(
            kind: .transcriptHypothesis,
            declared: 65_537,
            maximum: 65_536
        )
    ) {
        try ChildProtocol.encode(oversized)
    }
}

@Test
func maximumCompleteAudioPayloadRoundTrips() throws {
    let frame = ChildFrame(
        audioSessionID: 1,
        frame: try PCMFrame(
            turnID: 2,
            generationID: 3,
            utteranceID: 4,
            sequence: 5,
            format: PCMFormat(
                sampleRateHz: 24_000,
                channels: 1,
                sampleFormat: .signed16LittleEndian
            ),
            bytes: Data(repeating: 0, count: 65_536)
        )
    )
    let encoded = try ChildProtocol.encode(frame)

    #expect(ChildProtocol.audioMetadataBytes == 48)
    #expect(encoded.count == 8 + 65_584)
    #expect(try ChildProtocol.decode(encoded) == frame)
}

@Test
func audioBoundsAreValidatedBeforeAllocation() {
    #expect(
        throws: ChildProtocolError.payloadTooLarge(
            kind: .audioFrame,
            declared: 65_585,
            maximum: 65_584
        )
    ) {
        try ChildProtocol.decode(header(kind: .audioFrame, payloadLength: 65_585))
    }
    #expect(
        throws: ChildProtocolError.pcmPayloadTooLarge(declared: 65_537, maximum: 65_536)
    ) {
        try ChildProtocol.decodeAudioPayload(Data(repeating: 0, count: 48 + 65_537))
    }
}

@Test
func declaredUInt32MaximumFailsWithoutReadingPayload() {
    #expect(
        throws: ChildProtocolError.payloadTooLarge(
            kind: .startSession,
            declared: Int(UInt32.max),
            maximum: 65_536
        )
    ) {
        try ChildProtocol.decode(header(kind: .startSession, payloadLength: UInt32.max))
    }
}

@Test
func audioMetadataRejectsUnsupportedAndInvalidFormats() {
    var unknownFormat = validAudioPayload()
    unknownFormat.replaceSubrange(46..<48, with: Data([0, 3]))
    #expect(throws: ChildProtocolError.unknownSampleFormat(3)) {
        try ChildProtocol.decode(rawFrame(kind: .audioFrame, payload: unknownFormat))
    }

    var zeroRate = validAudioPayload()
    zeroRate.replaceSubrange(40..<44, with: Data(repeating: 0, count: 4))
    #expect(
        throws: ChildProtocolError.invalidAudioMetadata(
            "PCM sample rate must be greater than zero"
        )
    ) {
        try ChildProtocol.decodeAudioPayload(zeroRate)
    }

    var zeroChannels = validAudioPayload()
    zeroChannels.replaceSubrange(44..<46, with: Data(repeating: 0, count: 2))
    #expect(
        throws: ChildProtocolError.invalidAudioMetadata(
            "PCM channels must be greater than zero"
        )
    ) {
        try ChildProtocol.decodeAudioPayload(zeroChannels)
    }

    var unaligned = validAudioPayload()
    unaligned.append(0)
    #expect(
        throws: ChildProtocolError.invalidAudioMetadata(
            "PCM frame bytes were not sample aligned"
        )
    ) {
        try ChildProtocol.decodeAudioPayload(unaligned)
    }
}

@Test
func failurePayloadIsTypedContentFreeAndStrict() throws {
    let frame = ChildFrame(
        control: .failure(
            sessionID: 9,
            stage: .speechRecognizer,
            code: .permissionDenied
        )
    )
    let encoded = try ChildProtocol.encode(frame)

    #expect(
        encoded.dropFirst(8)
            == Data(
                #"{"session_id":9,"stage":"speech_recognizer","code":"permission_denied"}"#.utf8
            )
    )
    #expect(try ChildProtocol.decode(encoded) == frame)

    for field in ["message", "text", "transcript", "content"] {
        let payload = Data(
            """
            {"session_id":9,"stage":"speech_recognizer","code":"permission_denied","\(field)":"private transcript"}
            """.utf8
        )
        #expect(throws: ChildProtocolError.invalidControlJSON) {
            try ChildProtocol.decode(rawFrame(kind: .failure, payload: payload))
        }
    }
}

@Test
func everyControlKindRoundTripsWithExactIdentity() throws {
    let controls: [ChildControl] = [
        .startSession(sessionID: 1, speechStartMilliseconds: 200, finalSilenceMilliseconds: 600),
        .startCapture(sessionID: 1, operationID: 1),
        .flushGeneration(sessionID: 1, generationID: 2, operationID: 3),
        .shutdown(sessionID: 1),
        .ready(sessionID: 1),
        .voiceActivity(
            sessionID: 1,
            activity: .speechStarted(atMilliseconds: 10)
        ),
        .voiceActivity(
            sessionID: 1,
            activity: .speechContinued(atMilliseconds: 20)
        ),
        .voiceActivity(
            sessionID: 1,
            activity: .speechEnded(atMilliseconds: 30)
        ),
        .transcriptHypothesis(
            sessionID: 1,
            hypothesis: RecognitionHypothesis(segmentID: 2, text: "hello", engineFinal: true)
        ),
        .playbackAccepted(
            sessionID: 1,
            turnID: 2,
            generationID: 3,
            utteranceID: 4,
            sequence: 5
        ),
        .playbackRendered(
            sessionID: 1,
            turnID: 2,
            generationID: 3,
            utteranceID: 4,
            sequence: 5
        ),
        .playbackFlushed(sessionID: 1, generationID: 2, operationID: 3),
        .failure(sessionID: 1, stage: .audioOutput, code: .playbackFailed),
        .shutdownComplete(sessionID: 1),
    ]

    for control in controls {
        let frame = ChildFrame(control: control)
        #expect(try ChildProtocol.decode(ChildProtocol.encode(frame)) == frame)
    }
}

@Test
func trailingBytesAreRejected() {
    var data = rawFrame(kind: .startCapture, payload: Data(#"{"session_id":7}"#.utf8))
    data.append(0)

    #expect(throws: ChildProtocolError.trailingBytes(1)) {
        try ChildProtocol.decode(data)
    }
}

private func rawFrame(
    version: UInt16 = ChildProtocol.version,
    kind: ChildFrameKind,
    payload: Data
) -> Data {
    rawFrame(version: version, kindCode: kind.rawValue, payload: payload)
}

private func rawFrame(
    version: UInt16 = ChildProtocol.version,
    kindCode: UInt16,
    payload: Data
) -> Data {
    var data = Data()
    data.appendBigEndian(version)
    data.appendBigEndian(kindCode)
    data.appendBigEndian(UInt32(payload.count))
    data.append(payload)
    return data
}

private func header(kind: ChildFrameKind, payloadLength: UInt32) -> Data {
    var data = Data()
    data.appendBigEndian(ChildProtocol.version)
    data.appendBigEndian(kind.rawValue)
    data.appendBigEndian(payloadLength)
    return data
}

private func validAudioPayload() -> Data {
    var data = Data()
    data.appendBigEndian(UInt64(1))
    data.appendBigEndian(UInt64(2))
    data.appendBigEndian(UInt64(3))
    data.appendBigEndian(UInt64(4))
    data.appendBigEndian(UInt64(5))
    data.appendBigEndian(UInt32(24_000))
    data.appendBigEndian(UInt16(1))
    data.appendBigEndian(UInt16(1))
    data.append(Data([0x00, 0x80, 0xFF, 0x7F]))
    return data
}
