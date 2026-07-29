public struct BargeInGate: Equatable, Sendable {
    public let thresholdMilliseconds: UInt64

    private var accumulatedMilliseconds: UInt64 = 0
    private var triggered = false

    public init(thresholdMilliseconds: UInt64) {
        self.thresholdMilliseconds = thresholdMilliseconds
    }

    public mutating func observe(
        isSpeech: Bool,
        frameMilliseconds: UInt64
    ) -> Bool {
        guard isSpeech else {
            reset()
            return false
        }
        guard !triggered else {
            return false
        }

        accumulatedMilliseconds = accumulatedMilliseconds.addingReportingOverflow(
            frameMilliseconds
        ).overflow
            ? UInt64.max
            : accumulatedMilliseconds + frameMilliseconds
        guard accumulatedMilliseconds >= thresholdMilliseconds else {
            return false
        }

        triggered = true
        return true
    }

    public mutating func reset() {
        accumulatedMilliseconds = 0
        triggered = false
    }
}
