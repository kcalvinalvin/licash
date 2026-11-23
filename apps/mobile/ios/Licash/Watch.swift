import Foundation
import LicashCore

// What keeps the app at work out of sight, and what it tells of then. A
// payment under way finishes only while the app runs: a payout's coin is
// claimed before its deadline, notes locked to the wallet are claimed, a
// deposit is swapped in. Pure, apart from iOS: the engine acts on it.

/// How closely to watch what is under way.
enum Watch {
    /// Nothing under way.
    case none

    /// Nothing urgent: a look now and then.
    case later

    /// A payment the app finishes: kept at work, in a background task.
    case now
}

/// Lines under way for longer are stuck, their hub or provider gone: nothing
/// left to finish in time, and not to keep the app at work for ever.
let STALE_SECS: Int64 = 7 * 24 * 3600

/// A Lightning invoice asked is looked for so long after: perhaps paid out of sight.
let ASKED_SECS: Int64 = 24 * 3600

/// How closely to watch what `ui` shows under way, at `now` in seconds since the epoch.
func watchOf(ui: Ui, now: Int64) -> Watch {
    let contracting = ui.opening != nil || ui.balance.moving > 0 ||
        ui.contracts.contains { $0.state == .opening || $0.state == .confirming }
    // Of unknown age: perhaps not stuck.
    let under = ui.history.filter { $0.state == .pending && ($0.at ?? now) > now - STALE_SECS }
    let asked = ui.asked.map { $0 > now - ASKED_SECS } ?? false
    // A forced close moves on with blocks, an hour or a day apart: a look now and then.
    let forcing = ui.recoveries.contains { carriedOn(recovery: $0) }
    // A proof posted: a challenge of it is answered within hours, not when a periodic job comes.
    let disputed = ui.recoveries.contains { DISPUTED.contains($0.stage) }
    // Mailed to its payee within the hour unless collected at its hub: a look now and then does.
    let delivering = ui.onTheirWay > 0
    // A contract protecting nears its end by blocks: its exit is looked at now and then.
    let protecting = ui.contracts.contains { $0.state == .protecting }
    if contracting || disputed || under.contains(where: { $0.kind != .optimized && !unpaid(activity: $0, details: ui.under[$0.key]) }) {
        return .now
    }
    if !under.isEmpty || asked || forcing || delivering || protecting || ui.noteMaintenance { return .later }
    return Watch.none
}

/// Whether the app is behind Bitcoin: its light client downloading or
/// catching up, or the wallet's payments not looked for up to the blocks
/// its hubs index. Waiting for a hub to be chosen is not behind.
func behind(ui: Ui) -> Bool {
    if ui.chain.importing >= 0 || !ui.chain.synced { return true }
    if ui.hubs.isEmpty { return false }
    guard let scan = ui.scan else { return true }
    return scan.next <= scan.height
}

/// A forced close's stages from the hub's answer until paid: kept at work, the hub's proof to check and challenge in time.
let DISPUTED: Set<RecoveryStage> = [
    .answered,
    .asserted,
    .challenging,
    .challenged,
]

/// A notification of a contract's exit or forced close: `key` told once, `urgent` asking her to act or stay online.
struct ExitNotice: Equatable {
    var key: String
    var urgent: Bool
    var text: String
}

/// What to tell of the contracts' `exits` this round and of the forced
/// closes `recoveries` disputed or held back by their claim's fee: what the
/// exit did, what she must do, and when to keep the app online. Amounts
/// only, never a hub's name.
func exitNotices(exits: [ContractExit], recoveries: [Recovery]) -> [ExitNotice] {
    let told = exits.compactMap { e -> ExitNotice? in
        // Its step in capitals, its words joined by underscores: NEEDS_CONSENT.
        let step = "\(e.step)".map { $0.isUppercase ? "_\($0)" : "\($0)" }.joined().uppercased()
        let key = "exit \(e.id) \(step)"
        let done = e.error == nil
        switch e.step {
        case .needsConsent:
            return ExitNotice(
                key: key,
                urgent: true,
                text: e.owed == 0
                    ? "Protection of \(sat(amount: e.uncovered)) ends soon. Allow exits in Settings, Recovery safety, " +
                        "or pay those notes out yourself."
                    : "Protection of \(sat(amount: e.uncovered)), and \(sat(amount: e.owed)) of payments owed to you that only " +
                        "forcing it closed recovers, ends soon. Allow exits in Settings, Recovery safety."
            )
        case .neverFitted:
            return ExitNotice(
                key: key,
                urgent: true,
                text: "A contract ends sooner than your exit margin allows. Pay its notes out or force close it yourself."
            )
        case .cashingOut:
            return done ? ExitNotice(key: key, urgent: false, text: "Paying out \(sat(amount: e.uncovered)) to your address as protection ends") : nil
        case .folding:
            return done ? ExitNotice(key: key, urgent: false, text: "Paid out, a contract closed with its hub: what was left of your notes and its reserve are on their way back") : nil
        case .forceClosing:
            return done
                ? ExitNotice(key: key, urgent: true, text: "Not paid out in time: a contract forced closed. Keep Licash online until it is done.")
                : ExitNotice(key: "\(key) failed", urgent: true, text: "A contract ending could not be forced closed. Open Licash to try again.")
        case .waiting, .covered, .paying:
            return nil
        }
    }
    let disputed = recoveries.filter { DISPUTED.contains($0.stage) }.map {
        ExitNotice(
            key: "dispute \($0.id)",
            urgent: true,
            text: "A hub answered a forced close. Keep Licash online with a connection until it is done, " +
                "so a false proof is challenged in time."
        )
    }
    // Its claim's window short: told at once.
    let capped = recoveries.filter { $0.feeCapped && carriedOn(recovery: $0) }.map {
        ExitNotice(
            key: "fee \($0.id)",
            urgent: true,
            text: "A forced close needs a higher fee than its reserve pays to get its claim on Bitcoin. " +
                "Open Licash and allow it within the hour."
        )
    }
    return told + disputed + capped
}

/// Whether `contract` still has steps to carry on: being opened or
/// confirmed, or protecting while the notes she funded it with are not in
/// her balance yet, issued only once its funding is buried.
func carriedOn(contract: Contract) -> Bool {
    switch contract.state {
    case .opening, .confirming: return true
    case .protecting: return contract.funded > 0 && !contract.fundedReceived
    default: return false
    }
}

/// Whether `recovery` still has steps to carry on.
func carriedOn(recovery: Recovery) -> Bool {
    ![RecoveryStage.done, .lost].contains(recovery.stage)
}

/// A deposit whose own payment on Bitcoin was never sent: it ends only once
/// its hub cancels it. Where it stands unread, perhaps sent.
func unpaid(activity: Activity, details: Details?) -> Bool {
    guard activity.kind == .moved, case let .moved(_, _, _, _, payment, _, _, _, _)? = details else { return false }
    return payment == nil
}

/// The lines finished since last looked, and those under way now, by [key].
struct Look: Equatable {
    var finished: [Activity]
    var under: Set<String>
}

/// Compare `history` with the lines under way `before`, and with every line
/// `seen` last time if known. Never looked before (`before` nil, as on the
/// first start of an app that did not keep them): none finished, so nothing
/// done long ago is told of as new. A Lightning invoice paid and its notes
/// taken at once, never seen under way, finished too if not `seen` before.
func look(history: [Activity], before: Set<String>?, seen: Set<String>? = nil) -> Look {
    let under = Set(history.filter { $0.state == .pending }.map { $0.key })
    let finished = before.map { b in
        history.filter {
            $0.state != .pending &&
                (b.contains($0.key) || (seen != nil && $0.kind == .invoice && !seen!.contains($0.key)))
        }
    }
    return Look(finished: finished ?? [], under: under)
}

/// What to tell of a line finished, if anything, in a notification: amounts
/// only, never why one failed (it may name a hub or a transaction). A coin
/// moving in shows as its deposit.
func news(activity: Activity) -> String? {
    let amount = sat(amount: activity.amount)
    switch activity.state {
    case .pending:
        return nil
    case .done:
        switch activity.kind {
        case .sent: return "Sent \(amount)"
        case .lightning: return "Paid \(amount) over Lightning"
        case .payout: return "Sent \(amount) on Bitcoin"
        case .swap: return "Moved \(amount) between hubs"
        case .moved: return "\(amount) added to your balance"
        case .invoice: return "Received \(amount) over Lightning"
        case .contract: return "Protection on"
        case .received, .funded, .onchain, .optimized, .closed: return nil
        }
    case .failed:
        let doing: String? = switch activity.kind {
        case .sent: "Sending \(amount)"
        case .lightning: "Paying \(amount) over Lightning"
        case .payout: "Sending \(amount) on Bitcoin"
        case .swap: "Moving \(amount) between hubs"
        case .moved: "Adding \(amount) to your balance"
        case .invoice: "Receiving \(amount) over Lightning"
        case .contract: "Setting up protection"
        case .received, .funded, .onchain, .optimized, .closed: nil
        }
        return doing.map { "\($0) failed: open Licash for why" }
    }
}

/// Blocks before a payout's deadline to warn at: about two hours.
let WARN_BLOCKS: UInt32 = 12

/// The payouts whose provider sent its coin, not claimed yet within
/// [WARN_BLOCKS] of the block to claim it by: that block, by [key]. Past it,
/// the provider may take its coin back and the payout fails.
func nearing(ui: Ui) -> [String: UInt32] {
    var near: [String: UInt32] = [:]
    for activity in ui.history {
        if activity.kind != .payout || activity.state != .pending { continue }
        guard case let .payout(_, _, _, _, _, _, steps, _, _, _)? = ui.under[activity.key] else { continue }
        guard let by = steps.claimBy else { continue }
        let tip = max(steps.tip, UInt32(clamping: ui.chain.height))
        if steps.coin != nil && steps.claim == nil && tip + WARN_BLOCKS >= by { near[activity.key] = by }
    }
    return near
}
