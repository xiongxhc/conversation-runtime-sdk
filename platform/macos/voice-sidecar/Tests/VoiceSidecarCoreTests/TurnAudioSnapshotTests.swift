import Testing
@testable import VoiceSidecarCore

/// A sample whose 16-bit conversion is `value` exactly.
private func exactSample(_ value: Int) -> Float {
    Float(value) / 32_767
}

@Test
func dropsTheClosingSilenceAndKeepsTheLastTenSeconds() {
    let turn = ContiguousArray((0..<200_000).map { exactSample($0 % 30_000) })

    let samples = TurnAudioSnapshot.pcm16(
        turnSamples: turn,
        finalSilenceMilliseconds: 700
    )

    // 700 ms at 16 kHz is 11,200 samples; the 160,000 before them remain.
    #expect(samples.count == 160_000)
    #expect(
        samples
            == (28_800..<188_800).map { Int16($0 % 30_000) }
    )
}

@Test
func keepsAShortTurnWhole() {
    let turn = ContiguousArray((0..<20_000).map { exactSample($0) })

    let samples = TurnAudioSnapshot.pcm16(
        turnSamples: turn,
        finalSilenceMilliseconds: 200
    )

    #expect(samples == (0..<16_800).map { Int16($0) })
}

@Test
func clampsAndScalesSamples() {
    let samples = TurnAudioSnapshot.pcm16(
        turnSamples: [0, 1, -1, 2, -2, 0.5],
        finalSilenceMilliseconds: 0
    )

    #expect(samples == [0, 32_767, -32_767, 32_767, -32_767, 16_384])
}

@Test
func aNaNSampleBecomesSilence() {
    let samples = TurnAudioSnapshot.pcm16(
        turnSamples: [.nan, 0.25],
        finalSilenceMilliseconds: 0
    )

    #expect(samples == [0, 8_192])
}

@Test
func aTurnShorterThanItsClosingSilenceIsEmpty() {
    let turn = ContiguousArray<Float>(repeating: 0.5, count: 11_200)

    #expect(
        TurnAudioSnapshot.pcm16(
            turnSamples: turn,
            finalSilenceMilliseconds: 700
        ).isEmpty
    )
    #expect(
        TurnAudioSnapshot.pcm16(
            turnSamples: [],
            finalSilenceMilliseconds: 700
        ).isEmpty
    )
}
