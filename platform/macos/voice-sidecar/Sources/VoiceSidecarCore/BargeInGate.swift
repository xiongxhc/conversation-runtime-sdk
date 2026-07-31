public struct BargeInGate: Equatable, Sendable {
    public static let windowMilliseconds: UInt64 = 100

    private let requiredConsecutiveWindows: Int
    private var consecutiveWindows = 0
    private var triggered = false

    public init(speechStartMilliseconds: UInt64 = 200) {
        let fullWindows =
            speechStartMilliseconds / Self.windowMilliseconds
        let roundedWindows =
            fullWindows
            + (speechStartMilliseconds % Self.windowMilliseconds == 0 ? 0 : 1)
        requiredConsecutiveWindows = max(1, Int(clamping: roundedWindows))
    }

    public mutating func observe(
        isSpeech: Bool,
        frameMilliseconds: UInt64
    ) -> Bool {
        guard isSpeech,
            frameMilliseconds == Self.windowMilliseconds
        else {
            reset()
            return false
        }
        guard !triggered else {
            return false
        }

        consecutiveWindows += 1
        guard consecutiveWindows == requiredConsecutiveWindows else {
            return false
        }

        triggered = true
        return true
    }

    public mutating func reset() {
        consecutiveWindows = 0
        triggered = false
    }
}
