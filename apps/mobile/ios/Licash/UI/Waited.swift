import Foundation
import Observation
import SwiftUI

/// A wait the user sees through: since when it lasts, while it does, and
/// how long the last one took once over. Times are monotonic clock ms,
/// sleep included.
@Observable
final class Wait {
    private(set) var since: Int64? = nil
    private(set) var took: Int64? = nil

    /// The wait starts, or ends, at `now`: told each time it may have.
    func turn(waiting: Bool, now: Int64) {
        let began = since
        if waiting && began == nil {
            since = now
            took = nil
        } else if !waiting, let began {
            took = now - began
            since = nil
        }
    }

    /// What [Waited] says at `now`: the seconds so far, how long it took, or nothing.
    func says(now: Int64) -> String? {
        if let since { return ticking(seconds: UInt64(clamping: (now - since) / 1000)) }
        if let took, took >= Wait.NOTICED_MS { return "Took \(lasted(ms: took))" }
        return nil
    }

    /// A wait shorter goes unsaid once over: nothing was waited on.
    private static let NOTICED_MS: Int64 = 100
}

/// `ms` as said once over: `0.4 s` and `9.9 s`, then as a clock ticks them.
func lasted(ms: Int64) -> String {
    ms < 10_000 ? "\(ms / 1000).\(ms % 1000 / 100) s" : ticking(seconds: UInt64(ms / 1000))
}

/// The monotonic clock in ms, counting while the phone sleeps.
private func elapsedRealtime() -> Int64 {
    Int64(clock_gettime_nsec_np(CLOCK_MONOTONIC) / 1_000_000)
}

extension View {
    /// The wait of a screen, `waiting` or not now: `wait`, which the screen
    /// keeps in its state, turned once each time `waiting` changes.
    func rememberWait(wait: Wait, waiting: Bool) -> some View {
        onChange(of: waiting, initial: true) { _, waiting in
            wait.turn(waiting: waiting, now: elapsedRealtime())
        }
    }
}

extension EnvironmentValues {
    /// The wait of the screen shown, for the parts all its steps share.
    var wait: Wait? {
        get { self[LocalWait.self] }
        set { self[LocalWait.self] = newValue }
    }
}

private struct LocalWait: EnvironmentKey {
    static let defaultValue: Wait? = nil
}

/// Softly, under what is waited on, the seconds it lasts, ticking. Once
/// over, how long it took. Nothing before any wait. Without a `wait`, the
/// screen's, from the environment.
struct Waited: View {
    private let given: Wait??
    let center: Bool
    @Environment(\.wait) private var local
    @State private var now = elapsedRealtime()

    init(wait: Wait?? = .none, center: Bool = false) {
        self.given = wait
        self.center = center
    }

    var body: some View {
        let wait = given ?? local
        let since = wait?.since
        if let wait, let says = wait.says(now: max(now, since ?? 0)) {
            Text(says)
                .textStyle(Theme.type.bodySmall)
                .foregroundStyle(Theme.color.onSurfaceVariant)
                .multilineTextAlignment(center ? .center : .leading)
                .accessibilityIdentifier("elapsed")
                .task(id: since) { await tick(since: since) }
        }
    }

    /// `now` kept to the whole seconds since `since`, while it waits.
    private func tick(since: Int64?) async {
        guard let since else { return }
        while true {
            let ms = 1000 - (elapsedRealtime() - since) % 1000
            do { try await Task.sleep(for: .milliseconds(ms)) } catch { return }
            now = elapsedRealtime()
        }
    }
}
