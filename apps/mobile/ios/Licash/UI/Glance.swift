import LicashCore

/// What Home says is going on, in one line at most, and what a tap on it
/// opens: a payment under way before anything else, then protection being
/// set up, then what kept the last sync short. Nothing going on, nothing
/// said. Catching up with Bitcoin is said by the connections ([links]).
enum Glance: Equatable {
    /// One payment under way: itself, on a tap. `coming`, on Bitcoin, in no block yet.
    case one(text: String, activity: Activity, coming: Bool)

    /// Several under way: the history, on a tap.
    case many(count: Int)

    /// Protection being set up at `hub`: where the money is, on a tap.
    case protecting(hub: String)

    /// The last look for payments fell short: `reason`, on a tap.
    case behind(reason: String)

    var text: String {
        switch self {
        case .one(let text, _, _): text
        case .many(let count): "\(count) in progress"
        case .protecting: "Setting up protection"
        case .behind: "Not up to date"
        }
    }
}

/// The line for `ui` at `now`, in seconds since 1970. Left out: a line under
/// way for longer than a week, stuck with nothing to wait for, and a deposit
/// whose payment on Bitcoin was never sent. The history keeps both.
func glance(ui: Ui, now: Int64) -> Glance? {
    let coming = ui.incoming.map { Glance.one(text: doing(activity: $0, coming: true), activity: $0, coming: true) }
    let under = ui.history
        .filter { $0.state == .pending && ($0.at ?? now) > now - STALE_SECS && !unpaid(activity: $0, details: ui.under[$0.key]) }
        .map { Glance.one(text: doing(activity: $0, coming: false), activity: $0, coming: false) }
    let lines = coming + under
    let opening = ui.opening
    if lines.count == 1 { return lines[0] }
    if lines.count > 1 { return .many(count: lines.count) }
    if let opening { return .protecting(hub: opening) }
    if let problem = ui.problem { return .behind(reason: problem) }
    return nil
}

/// What a payment under way is doing, with its amount: `Receiving 10,000 sat`.
func doing(activity: Activity, coming: Bool) -> String {
    if coming { return "Receiving \(sat(amount: activity.amount))" }
    switch activity.kind {
    case .sent, .lightning, .payout: return "Sending \(sat(amount: activity.amount))"
    case .received, .invoice, .moved, .funded, .onchain, .closed: return "Receiving \(sat(amount: activity.amount))"
    case .swap: return "Moving \(sat(amount: activity.amount))"
    case .contract: return "Setting up protection"
    case .optimized: return "Optimizing notes"
    }
}
