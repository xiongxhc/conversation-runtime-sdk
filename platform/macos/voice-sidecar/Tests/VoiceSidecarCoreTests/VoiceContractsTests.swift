import Testing
@testable import VoiceSidecarCore

@Test
func audioDeviceLabelsAreTrimmedAtTheSource() {
    let status = AudioDeviceStatus(
        inputLabel: "USB Audio Device \n",
        outputLabel: "\t Speakers "
    )

    #expect(status.inputLabel == "USB Audio Device")
    #expect(status.outputLabel == "Speakers")
}

@Test
func blankAudioDeviceLabelsNormalizeToEmpty() {
    let status = AudioDeviceStatus(inputLabel: "   ", outputLabel: "\n")

    #expect(status.inputLabel.isEmpty)
    #expect(status.outputLabel.isEmpty)
}

@Test
func oversizedAudioDeviceLabelsTruncateOnCharacterBoundaries() {
    let name = String(repeating: "麦", count: 60)

    let status = AudioDeviceStatus(inputLabel: name, outputLabel: "Speakers")

    #expect(status.inputLabel.utf8.count <= AudioDeviceStatus.maximumLabelBytes)
    #expect(status.inputLabel == String(repeating: "麦", count: 42))
    #expect(name.hasPrefix(status.inputLabel))
}
