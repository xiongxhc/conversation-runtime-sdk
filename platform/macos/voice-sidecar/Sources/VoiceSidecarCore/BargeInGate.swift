public struct BargeInGate: Equatable, Sendable {
    public static let windowMilliseconds: UInt64 = 100
    public static let requiredConsecutiveWindows = 2

    private var consecutiveWindows = 0
    private var triggered = false

    public init() {}

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
        guard consecutiveWindows == Self.requiredConsecutiveWindows else {
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
