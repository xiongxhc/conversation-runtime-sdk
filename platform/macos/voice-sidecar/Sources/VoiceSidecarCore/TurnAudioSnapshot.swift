/// The audio a `TurnAudio` frame carries for a finished turn.
public enum TurnAudioSnapshot {
    private static let samplesPerMillisecond = 16

    /// The turn's 16 kHz mono buffer without the final silence that ended it,
    /// limited to its last 160,000 samples, clamped to [-1, 1], scaled by
    /// 32,767 and rounded to the nearest integer.
    public static func pcm16(
        turnSamples: ContiguousArray<Float>,
        finalSilenceMilliseconds: UInt64
    ) -> [Int16] {
        let silence = finalSilenceMilliseconds
            .multipliedReportingOverflow(by: UInt64(samplesPerMillisecond))
        guard !silence.overflow,
            silence.partialValue < UInt64(turnSamples.count)
        else {
            return []
        }
        let end = turnSamples.count - Int(silence.partialValue)
        let start = max(0, end - ChildProtocol.maximumTurnAudioSamples)
        return turnSamples[start..<end].map { sample in
            // Capture never yields NaN; if it did, silence is the safe sample.
            guard !sample.isNaN else {
                return 0
            }
            return Int16((min(max(sample, -1), 1) * 32_767).rounded())
        }
    }
}
