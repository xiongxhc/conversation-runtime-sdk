import Testing
@testable import VoiceSidecarCore

@Test
func twoHundredMillisecondsOfSpeechFlushesCurrentGeneration() {
    var gate = BargeInGate(thresholdMilliseconds: 200)

    #expect(gate.observe(isSpeech: true, frameMilliseconds: 100) == false)
    #expect(gate.observe(isSpeech: true, frameMilliseconds: 100) == true)
    #expect(gate.observe(isSpeech: true, frameMilliseconds: 100) == false)
}

@Test
func speechGapResetsAccumulationAndTriggerLatch() {
    var gate = BargeInGate(thresholdMilliseconds: 200)

    #expect(gate.observe(isSpeech: true, frameMilliseconds: 100) == false)
    #expect(gate.observe(isSpeech: false, frameMilliseconds: 100) == false)
    #expect(gate.observe(isSpeech: true, frameMilliseconds: 100) == false)
    #expect(gate.observe(isSpeech: true, frameMilliseconds: 100) == true)
    #expect(gate.observe(isSpeech: false, frameMilliseconds: 100) == false)
    #expect(gate.observe(isSpeech: true, frameMilliseconds: 200) == true)
}
