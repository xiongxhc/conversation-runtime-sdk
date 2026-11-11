import Testing
@testable import VoiceSidecarCore

/// A sample whose 16-bit conversion is `value` exactly.
private func exactSample(_ value: Int) -> Float {
    Float(value) / 32_767
}

@Test
func keepsTheLastTenSecondsOfTheSpeech() {
    let speech = ContiguousArray((0..<200_000).map { exactSample($0 % 30_000) })

    let samples = TurnAudioSnapshot.pcm16(speech)

    #expect(samples.count == 160_000)
    #expect(samples == (40_000..<200_000).map { Int16($0 % 30_000) })
}

@Test
func keepsShortSpeechWhole() {
    let speech = ContiguousArray((0..<20_000).map { exactSample($0) })

    #expect(TurnAudioSnapshot.pcm16(speech) == (0..<20_000).map { Int16($0) })
}

@Test
func clampsAndScalesSamples() {
    let samples = TurnAudioSnapshot.pcm16([0, 1, -1, 2, -2, 0.5])

    #expect(samples == [0, 32_767, -32_767, 32_767, -32_767, 16_384])
}

@Test
func aNaNSampleBecomesSilence() {
    #expect(TurnAudioSnapshot.pcm16([.nan, 0.25]) == [0, 8_192])
}

@Test
func theSpeechRunsFromTheLeadBeforeTheFirstStartToTheLastEnd() {
    // The buffer starts at position 1,000; speech started at 201,000 and
    // ended at 225,000, with an 8,000-sample lead.
    #expect(
        TurnAudioSnapshot.speechRange(
            bufferStart: 1_000,
            bufferCount: 300_000,
            firstSpeechStart: 201_000,
            lastSpeechEnd: 225_000,
            leadSamples: 8_000
        ) == 192_000..<224_000
    )
}

@Test
func longSpeechKeepsItsLastTenSeconds() {
    #expect(
        TurnAudioSnapshot.speechRange(
            bufferStart: 0,
            bufferCount: 400_000,
            firstSpeechStart: 10_000,
            lastSpeechEnd: 350_000,
            leadSamples: 8_000
        ) == 190_000..<350_000
    )
}

@Test
func aLeadOrStartBeforeTheBufferIsCutAtTheBuffer() {
    #expect(
        TurnAudioSnapshot.speechRange(
            bufferStart: 5_000,
            bufferCount: 20_000,
            firstSpeechStart: 6_000,
            lastSpeechEnd: 15_000,
            leadSamples: 8_000
        ) == 0..<10_000
    )
    // A lead longer than the whole capture so far.
    #expect(
        TurnAudioSnapshot.speechRange(
            bufferStart: 0,
            bufferCount: 20_000,
            firstSpeechStart: 3_000,
            lastSpeechEnd: 15_000,
            leadSamples: 8_000
        ) == 0..<15_000
    )
}

@Test
func anEndBeyondTheBufferIsCutAtTheBuffer() {
    #expect(
        TurnAudioSnapshot.speechRange(
            bufferStart: 0,
            bufferCount: 20_000,
            firstSpeechStart: 10_000,
            lastSpeechEnd: 30_000,
            leadSamples: 0
        ) == 10_000..<20_000
    )
}

@Test
func aTurnWithoutSpeechHasNoRange() {
    for (start, end): (UInt64?, UInt64?) in [
        (nil, nil), (nil, 15_000), (10_000, nil), (10_000, 10_000), (12_000, 10_000),
    ] {
        #expect(
            TurnAudioSnapshot.speechRange(
                bufferStart: 0,
                bufferCount: 20_000,
                firstSpeechStart: start,
                lastSpeechEnd: end,
                leadSamples: 8_000
            ) == nil
        )
    }
    // Speech that ended before the buffer began.
    #expect(
        TurnAudioSnapshot.speechRange(
            bufferStart: 50_000,
            bufferCount: 20_000,
            firstSpeechStart: 10_000,
            lastSpeechEnd: 30_000,
            leadSamples: 0
        ) == nil
    )
}

@Test
func printingTurnAudioNeverPrintsItsSamples() {
    let frame = TurnAudioFrame(
        sessionID: 7,
        segmentID: 3,
        finalSegments: 1,
        playbackOverlap: true,
        samples: [12_345, -23_456]
    )
    let end = RecognitionTurnEnd(
        samples: [0.123_45, -0.234_56],
        nextTurnFromMilliseconds: 900,
        discontinuities: 0
    )
    var dumped = ""
    dump(ChildFrame(turnAudio: frame), to: &dumped)
    dump(end, to: &dumped)
    for printed in [
        String(describing: frame), String(reflecting: frame),
        String(describing: end), String(reflecting: end),
        String(describing: ChildFrame(turnAudio: frame)), dumped,
    ] {
        #expect(!printed.contains("12345"))
        #expect(!printed.contains("23456"))
        #expect(!printed.contains("0.123"))
        #expect(!printed.contains("0.234"))
    }
    #expect(String(describing: frame).contains("sampleCount: 2"))
}
