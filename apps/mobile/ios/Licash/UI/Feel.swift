import UIKit

/*
 * What the app feels like: a short haptic for an outcome, done or failed,
 * and a tick for a switch. Never on every tap, and never alone: the screen
 * says the same in words. Through UIKit's feedback generators, so the
 * system's setting for haptics is kept, and no permission is asked.
 */

/// What is felt.
enum Touch { case done, failed, on, off }

/// What iOS plays.
enum Haptic { case success, error, selection }

/// What iOS plays for `touch`: a notification's success or error for an
/// outcome, and the selection's tick for a switch, on or off.
func haptic(touch: Touch) -> Haptic {
    switch touch {
    case .done: .success
    case .failed: .error
    case .on, .off: .selection
    }
}

/// Felt through UIKit's feedback generators.
@MainActor
final class Feel {
    private let outcome = UINotificationFeedbackGenerator()
    private let tick = UISelectionFeedbackGenerator()

    func confirm() { feel(touch: .done) }

    func reject() { feel(touch: .failed) }

    /// A switch turned `on`, or off.
    func toggle(on: Bool) { feel(touch: on ? .on : .off) }

    private func feel(touch: Touch) {
        switch haptic(touch: touch) {
        case .success: outcome.notificationOccurred(.success)
        case .error: outcome.notificationOccurred(.error)
        case .selection: tick.selectionChanged()
        }
    }
}

/// What the screen shown feels like.
@MainActor
func rememberFeel() -> Feel { Feel() }
