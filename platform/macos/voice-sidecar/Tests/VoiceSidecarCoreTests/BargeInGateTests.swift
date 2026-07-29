import Testing
@testable import VoiceSidecarCore

@Test
func twoHundredMillisecondsOfSpeechFlushesCurrentGeneration() {
    var gate = BargeInGate()

    #expect(gate.observe(isSpeech: true, frameMilliseconds: 100) == false)
    #expect(gate.observe(isSpeech: true, frameMilliseconds: 100) == true)
    #expect(gate.observe(isSpeech: true, frameMilliseconds: 100) == false)
}

@Test
func speechGapResetsAccumulationAndTriggerLatch() {
    var gate = BargeInGate()

    #expect(gate.observe(isSpeech: true, frameMilliseconds: 100) == false)
    #expect(gate.observe(isSpeech: false, frameMilliseconds: 100) == false)
    #expect(gate.observe(isSpeech: true, frameMilliseconds: 100) == false)
    #expect(gate.observe(isSpeech: true, frameMilliseconds: 100) == true)
    #expect(gate.observe(isSpeech: false, frameMilliseconds: 100) == false)
    #expect(gate.observe(isSpeech: true, frameMilliseconds: 200) == false)
    #expect(gate.observe(isSpeech: true, frameMilliseconds: 100) == false)
    #expect(gate.observe(isSpeech: true, frameMilliseconds: 100) == true)
}
