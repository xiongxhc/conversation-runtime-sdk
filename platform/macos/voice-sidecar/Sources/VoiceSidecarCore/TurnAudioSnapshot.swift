/// The audio a `TurnAudio` frame carries for a finished turn.
public enum TurnAudioSnapshot {
    /// The part of a turn buffer that holds the turn's speech: from the
    /// turn's first speech start, moved earlier by `leadSamples`, to its last
    /// speech end, then at most the last 160,000 samples of that span.
    /// Positions count samples on one capture clock, and `bufferStart` is the
    /// position of the buffer's first sample. Nil when the turn has no speech
    /// start, no speech end after it, or no buffered sample in the span.
    public static func speechRange(
        bufferStart: UInt64,
        bufferCount: Int,
        firstSpeechStart: UInt64?,
        lastSpeechEnd: UInt64?,
        leadSamples: UInt64
    ) -> Range<Int>? {
        guard let start = firstSpeechStart,
            let end = lastSpeechEnd,
            end > start
        else {
            return nil
        }
        func index(_ position: UInt64) -> Int {
            guard position > bufferStart else {
                return 0
            }
            return Int(min(position - bufferStart, UInt64(max(0, bufferCount))))
        }
        let upper = index(end)
        let lower = max(
            index(start > leadSamples ? start - leadSamples : 0),
            upper - ChildProtocol.maximumTurnAudioSamples
        )
        guard lower < upper else {
            return nil
        }
        return lower..<upper
    }

    /// A turn's speech as a backend cut it, limited to its last 160,000
    /// samples, clamped to [-1, 1], scaled by 32,767 and rounded to the
    /// nearest integer.
    public static func pcm16(_ speech: ContiguousArray<Float>) -> [Int16] {
        speech.suffix(ChildProtocol.maximumTurnAudioSamples).map { sample in
            // Capture never yields NaN; if it did, silence is the safe sample.
            guard !sample.isNaN else {
                return 0
            }
            return Int16((min(max(sample, -1), 1) * 32_767).rounded())
        }
    }
}
