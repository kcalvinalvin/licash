import Foundation
import LicashCore

/*
 * The app's words, where a screen does not own them: what is said the same
 * way wherever it shows. Protected, of what the wallet holds, is what is
 * its own whatever a hub does: on Bitcoin, or covered by a hub's
 * collateral. The rest is not protected: owed by a hub, on its word alone.
 * Pure, to be tested without a screen.
 */

/// How much of what the wallet holds is protected: all, some or none of it.
enum Shield { case all, some, none }

/// Of what is protected, what is `bitcoin` held and what `contracts` cover: in sat.
struct SafeSplit: Equatable {
    var bitcoin: UInt64
    var contracts: UInt64
}

/// `share`'s protected part split into what is on Bitcoin, `onBitcoin` at most, and what contracts cover.
func safeSplit(share: Protection, onBitcoin: UInt64) -> SafeSplit {
    let bitcoin = min(share.protected, onBitcoin)
    return SafeSplit(bitcoin: bitcoin, contracts: share.protected - bitcoin)
}

/// What the wallet holds, as `protected` and `open`, the rest: in sat.
struct Protection: Equatable {
    var protected: UInt64
    var open: UInt64

    var total: UInt64 { protected + open }

    /// None while nothing is held: nothing to say of it.
    var shield: Shield? {
        if total == 0 { return nil }
        if open == 0 { return Shield.all }
        if protected == 0 { return Shield.none }
        return Shield.some
    }

    /// [protected] as a share of [total], in whole percent: never 0 with
    /// some protected, nor 100 with some not.
    var percent: Int {
        if total == 0 { return 0 }
        let rounded = Int((Double(protected) * 100 / Double(total)) + 0.5)
        if protected > 0 && rounded == 0 { return 1 }
        if open > 0 && rounded == 100 { return 99 }
        return rounded
    }

    /// In a word or two, as Home says it.
    var label: String? {
        guard let shield else { return nil }
        switch shield {
        case .all: return PROTECTED
        case .none: return NOT_PROTECTED
        case .some: return "\(percent)% protected"
        }
    }
}

/// Whom a line of the history was paid to (one made) or by: its contact, if known.
func counterpart(kind: Kind, contact: String?) -> String? {
    contact.map { kind == .sent ? "To \($0)" : "From \($0)" }
}

/// How much of `balance` is protected.
func protection(balance: Balance) -> Protection {
    Protection(protected: balance.total - balance.credit, open: balance.credit)
}

/// The protection Home says beside the balance: none while one is being set
/// up (`opening`), which Home says instead, so it never says both.
func homeProtection(balance: Balance, opening: Bool) -> Protection? {
    opening ? nil : protection(balance: balance)
}

let PROTECTED = "Protected"
let PROTECTED_BY_CONTRACTS = "Protected by contracts"
let NOT_PROTECTED = "Not protected"

/// What each means, where asked.
let PROTECTED_MEANS =
    "Notes claimable from a live recovery contract, should their hub fail you. Coverage counts whole eligible notes within each contract's payout and note limits. Recovering costs Bitcoin fees and needs the app opened before the contract's deadline."
let NOT_PROTECTED_MEANS =
    "Held on credit: a hub owes it to you, on its word alone. Protect it with a contract, paid with a coin " +
    "you hold on Bitcoin, or cash it out to Bitcoin."

/// What bitcoin held is, beside what is protected: safe already, needing no contract.
let SAFE_ONCHAIN_MEANS =
    "Bitcoin you hold yourself: safe whatever a hub does. Paying for a contract with a coin of it protects " +
    "your notes at that hub, and adds the rest of the coin to your balance."

/// What paying through a swap provider risks, said before paying: should it
/// decline or fail the payment, the sats wait `blocks` for their lock to
/// expire.
func refundWait(blocks: UInt32, blockSecs: UInt32 = BITCOIN_BLOCK_SECONDS) -> String {
    "If the provider declines or fails the payment, your sats come back by themselves, in about \(span(blocks: Int64(blocks), blockSecs: blockSecs))."
}

/// Where a payment through a swap provider that it declined or failed
/// stands: its sats back once its lock expires, `blocks` from now if known.
func notPaid(blocks: UInt32?, blockSecs: UInt32 = BITCOIN_BLOCK_SECONDS) -> String {
    guard let blocks, blocks != 0 else { return "Not paid. Your money comes back by itself." }
    return "Not paid. Your money comes back by itself in about \(span(blocks: Int64(blocks), blockSecs: blockSecs))."
}

/// Seconds a block takes on Bitcoin's networks, 10 minutes: Mutinynet's take 30.
let BITCOIN_BLOCK_SECONDS: UInt32 = 600

/// How long `blocks` take, `blockSecs` each (those of the wallet's network),
/// as a clock would say it: `6 days`. Never a block height: what a block is,
/// nobody needs to know.
func span(blocks: Int64, blockSecs: UInt32 = BITCOIN_BLOCK_SECONDS) -> String {
    let seconds = blocks * Int64(blockSecs)
    let minutes = (seconds + 30) / 60
    let hours = (seconds + 1800) / 3600
    let days = (seconds + 43200) / 86400
    if blocks <= 0 { return "now" }
    if seconds < 60 { return "\(seconds) sec" }
    if seconds < 3600 { return "\(minutes) min" }
    if hours < 48 { return hours == 1 ? "1 hour" : "\(hours) hours" }
    return "\(days) days"
}

/// The minutes `blocks` take, `blockSecs` each, rounded and at least one.
func minutes(blocks: Int64, blockSecs: UInt32 = BITCOIN_BLOCK_SECONDS) -> Int64 {
    max(1, (blocks * Int64(blockSecs) + 30) / 60)
}

/// When `seconds` since 1970 was, as short as `now` lets it be said: the
/// time today, `Yesterday`, `Nov 13`, and with its year once it is another.
func day(
    seconds: Int64,
    now: Int64,
    zone: TimeZone = .current,
    locale: Locale = .current
) -> String {
    var calendar = Calendar(identifier: .gregorian)
    calendar.timeZone = zone
    let at = Date(timeIntervalSince1970: TimeInterval(seconds))
    let today = Date(timeIntervalSince1970: TimeInterval(now))
    let formatter = DateFormatter()
    formatter.locale = locale
    formatter.calendar = calendar
    formatter.timeZone = zone
    let format = { (pattern: String) -> String in
        formatter.dateFormat = pattern
        return formatter.string(from: at)
    }
    if calendar.isDate(at, inSameDayAs: today) {
        formatter.timeStyle = .short
        formatter.dateStyle = .none
        return formatter.string(from: at)
    }
    if let yesterday = calendar.date(byAdding: .day, value: -1, to: today), calendar.isDate(at, inSameDayAs: yesterday) {
        return "Yesterday"
    }
    if calendar.component(.year, from: at) == calendar.component(.year, from: today) { return format("MMM d") }
    return format("MMM d, yyyy")
}

/// A hub's name, short, from its `url`: its host, with its port if it names
/// one. An onion service's by its first letters, the rest telling nobody
/// anything.
func name(url: String) -> String {
    let host = host(url: url)
    let bare = String(host.prefix { $0 != ":" })
    let onion = bare.hasSuffix(".onion") ? String(bare.dropLast(".onion".count)) : bare
    if onion.count > 12 && bare.hasSuffix(".onion") {
        return String(onion.prefix(8)) + "….onion" + String(host.dropFirst(bare.count))
    }
    return host
}

/// A fee, as short as it can be said: `None`, `10 sat`, `0.5%`, `10 sat + 0.5%`.
func charge(rate: Rate) -> String {
    if rate.base == 0 && rate.ppm == 0 { return "None" }
    if rate.ppm == 0 { return sat(amount: rate.base) }
    if rate.base == 0 { return percent(ppm: rate.ppm) }
    return "\(sat(amount: rate.base)) + \(percent(ppm: rate.ppm))"
}

/// What a hub charges a payment in Licash: its rate and, if it charges for
/// the notes a payment makes, that for each: `1 sat + 0.01% + 0.1 sat a note`.
func paying(hub: Hub) -> String {
    paying(rate: Rate(base: hub.feeBase, ppm: hub.feePpm), outputMsat: hub.feeOutputMsat)
}

/// [paying], of a `rate` and a fee of `outputMsat` thousandths of a sat for each note made.
func paying(rate: Rate, outputMsat: UInt64) -> String {
    if outputMsat == 0 { return charge(rate: rate) }
    let (whole, rest) = (outputMsat / 1_000, outputMsat % 1_000)
    var each = sat(amount: whole)
    if rest != 0 {
        var thousandths = String(format: "%03llu", rest)
        while thousandths.hasSuffix("0") { thousandths.removeLast() }
        each = "\(whole).\(thousandths) sat"
    }
    return rate.base == 0 && rate.ppm == 0 ? "\(each) a note" : "\(charge(rate: rate)) + \(each) a note"
}

/// A fee a hub changed since it was agreed to: what it is on, what it is `now`, and what was `agreed`.
struct Raise: Equatable {
    var what: String
    var now: String
    var agreed: String
}

/// What a hub charging more than agreed changed, to approve: each of its
/// fees that differs, the others left unsaid. Approving takes none lower
/// than agreed.
func raises(raised: FeesRaised) -> [Raise] {
    let (now, agreed) = (raised.charged, raised.agreed)
    func said(fees: LicashCore.Fees) -> String { paying(rate: fees.spend, outputMsat: fees.outputMsat) }
    let paid = Raise(what: "Paying", now: said(fees: now), agreed: said(fees: agreed))
    return [
        paid.now != paid.agreed ? paid : nil,
        now.lock != agreed.lock ? Raise(what: "Holding a payment", now: charge(rate: now.lock), agreed: charge(rate: agreed.lock)) : nil,
        now.refund != agreed.refund ? Raise(what: "Refunding one", now: charge(rate: now.refund), agreed: charge(rate: agreed.refund)) : nil,
    ].compactMap { $0 }
}

/// Why a hub's fees are the owner's to approve, said wherever they are.
let RAISED_MEANS = "This hub raised its fees since your protection there started. Nothing is paid there until you approve them."

/// An optimization moves no amount away: show only its fee, not `0 sat`.
func optimizationCost(fee: UInt64, state: State) -> String {
    if state == .failed { return "Not charged" }
    if fee == 0 { return "Free" }
    return "Fee \(sat(amount: fee))"
}

/// The most a hub charges on paying `amount`, said before the notes it
/// takes are known: its rate on the amount, and its fee on as many notes
/// as the amount and what is left of it can be made in.
func payingUpTo(hub: Hub, amount: UInt64) -> UInt64 {
    let notes = 2 * UInt64(64 - amount.leadingZeroBitCount)
    // The hub sets its own fee: wrapping arithmetic, never a trap.
    let made = (hub.feeOutputMsat &* notes &+ 500) / 1_000
    let due = feeDue(amount: amount, rate: Rate(base: hub.feeBase, ppm: hub.feePpm))
    return UInt64.max - due < made ? UInt64.max : due + made
}

/// What protection under the mock authority is worth, said wherever one shows.
let MOCK = "Test only: protects nothing"

/// A protection as a row says it: where it `stands`, in a word. Whether it
/// is `on`, protecting now, `runs`, protecting or about to, or `failed`.
/// Whether the hub's bitcoin is `held` behind it. How long it has `left` to
/// protect, and in how long it `ends`, the hub's bitcoin back to it, while
/// the chain's height is known.
struct Cover: Equatable {
    var stands: String
    var on: Bool
    var runs: Bool
    var failed: Bool
    var held: Bool
    var left: String?
    var ends: String?
}

/// `contract`, in words, at the chain's `height`: 0 while unknown.
func cover(contract: Contract, height: Int, blockSecs: UInt32 = BITCOIN_BLOCK_SECONDS) -> Cover {
    let state = contract.state
    let on = state == .protecting
    let runs = on || state == .opening || state == .confirming
    let held = on || state == .confirming || state == .ending
    let known = height > 0
    let stands = switch state {
    case .opening: "Setting up"
    case .confirming: "Confirming"
    // Keep the live status distinct from regtest's mock protection.
    case .protecting: contract.mock ? "Protecting" : "Live"
    case .ending: "Ending"
    case .closed: "Ended"
    case .failed: "Failed"
    }
    let failed = if case .failed = state { true } else { false }
    return Cover(
        stands: stands,
        on: on,
        runs: runs,
        failed: failed,
        held: held,
        left: runs && known ? "\(span(blocks: Int64(contract.protectsUntil) - Int64(height), blockSecs: blockSecs)) left" : nil,
        ends: (runs || held) && known ? span(blocks: Int64(contract.closesAt) - Int64(height), blockSecs: blockSecs) : nil
    )
}

/// A protection forced closed, or closed with its hub and awaiting the
/// fold that pays it, as its row says it: where it `stands`, in a
/// word, and what it waits for (`says`), at the chain's `height`, 0 while
/// unknown. Whether it may still be given up, and whether it failed.
struct Forcing: Equatable {
    var stands: String
    var says: String?
    var revocable: Bool
    var failed: Bool
}

/// `recovery`, in words, at the chain's `height`.
func forcing(recovery: Recovery, height: Int, blockSecs: UInt32 = BITCOIN_BLOCK_SECONDS) -> Forcing {
    let wait = height > 0 ? recovery.nextAt.map { span(blocks: Int64($0) - Int64(height), blockSecs: blockSecs) } : nil
    let revocable = recovery.stage == .claimed && !recovery.irrevocable
    let (stands, says): (String, String?) = switch recovery.stage {
    case .folding: ("Closing", "Waiting for the hub's transaction: your notes and reserve come back on Bitcoin together")
    case .claimed: ("Forcing closed", wait.map { "Final in \($0): it can be given up until then" })
    case .requesting: ("Requesting", "Asking on Bitcoin for what the hub holds")
    case .requested: ("Requested", wait.map { "The hub has \($0) to answer" } ?? "The hub may answer")
    case .timingOut: ("Paying back", wait.map { "Back on Bitcoin in \($0)" } ?? "The hub did not answer")
    case .sweeping: ("Sweeping", "To your Bitcoin, once in a block")
    case .done: ("Recovered", "\(sat(amount: recovery.received ?? recovery.claimed)) back on Bitcoin")
    case .answered: ("Answered", answered(recovery: recovery, height: height, blockSecs: blockSecs))
    case .lost: ("Lost", "Its bitcoin went elsewhere")
    case .asserted: ("Hub proved", wait.map { "\(owed(recovery: recovery)): paid in \($0)" } ?? "Being paid")
    case .challenging: ("Challenging", "The hub's proof does not check: challenging it on Bitcoin")
    case .challenged: ("Challenged", wait.map { "The hub has \($0) to answer the challenge" } ?? "Being paid")
    case .paid: ("Paying back", wait.map { "Back on Bitcoin in \($0)" } ?? "Paid on Bitcoin")
    }
    let failed = recovery.stage == .lost
    return Forcing(stands: stands, says: says, revocable: revocable, failed: failed)
}

/// What the hub says the claim is owed, in words.
private func owed(recovery: Recovery) -> String {
    recovery.corrected.map { "The hub says \(sat(amount: $0)) is owed" } ?? "The hub answered"
}

/// A forced close the hub answered: its proof awaited, by its deadline.
private func answered(recovery: Recovery, height: Int, blockSecs: UInt32) -> String {
    guard let deadline = recovery.deadline else { return owed(recovery: recovery) }
    let left = height > 0 ? span(blocks: Int64(deadline) - Int64(height), blockSecs: blockSecs) : nil
    return left.map { "\(owed(recovery: recovery)): it must prove so within \($0), or you are paid your claim" }
        ?? "\(owed(recovery: recovery)): it must prove so, or you are paid your claim"
}

/// The steps of setting a protection up, the last its payment confirming.
let SETUP_STEPS: Double = 7

/// How far setting a protection up is, in steps done of [SETUP_STEPS], and
/// what it waits on, in a word or two.
func setup(step: OpeningStep) -> (Double, String) {
    switch step {
    case .terms: return (0, "Asking the hub")
    case .preparing(let seconds): return (0.75, "Preparing it, a few minutes · \(ticking(seconds: seconds))")
    case .choosing: return (1.5, "The hub is choosing")
    case .uploading: return (3, "Sending to the hub")
    case .signatures: return (4, "Waiting for the hub")
    case .signing: return (5, "Signing")
    case .confirming(let confirmations): return (6, confirmations == 0 ? "Waiting for Bitcoin" : "Confirming")
    }
}

/// `seconds` as a clock ticks them: `45 s`, `2 min 05 s`.
func ticking(seconds: UInt64) -> String {
    let (m, s) = (Int64(bitPattern: seconds) / 60, Int64(bitPattern: seconds) % 60)
    return m == 0 ? "\(s) s" : "\(m) min \(String(format: "%02lld", s)) s"
}

/// What setting a protection up sent and got so far, its speed and about
/// how long the rest takes at it: `12 MB of about 100 MB · 1.5 MB/s · about
/// 1 min left`. Its speed once a second has passed, the rest once some is.
/// Neither while the wallet works out a step itself (`busy`), which its
/// seconds count too: what little went then said nothing of the network.
func transferred(t: Transfer, busy: Bool = false) -> String {
    var parts = ["\(megabytes(bytes: t.bytes)) of about \(megabytes(bytes: t.expected))"]
    if !busy && t.seconds > 0 && t.bytes > 0 {
        let rate = t.bytes / t.seconds
        // Under 0.1 MB/s, as over Tor at times, in kB/s: not 0.0 MB/s.
        parts.append(rate < 100_000 ? "\(rate / 1000) kB/s" : "\(megabytes(bytes: rate))/s")
        if t.expected > t.bytes && rate > 0 {
            let left = Int64(bitPattern: (t.expected - t.bytes) / rate)
            if left == 0 {
                parts.append("almost done")
            } else if left < 60 {
                parts.append("about \(left) s left")
            } else if left < 90 * 60 {
                parts.append("about \((left + 59) / 60) min left")
            } else {
                parts.append("about \((left + 1800) / 3600) h left")
            }
        }
    }
    return parts.joined(separator: " · ")
}

/// `bytes` in megabytes, to a tenth below 10: `0.4 MB`, `12 MB`.
private func megabytes(bytes: UInt64) -> String {
    let mb = Double(bytes) / 1_000_000
    if mb < 10 {
        // Tenths rounded half up: 250,000 bytes are 0.3 MB.
        let tenths = (bytes + 50_000) / 100_000
        return "\(tenths / 10).\(tenths % 10) MB"
    }
    return "\(Int64(mb.rounded())) MB"
}

/// Whether `contract` shows outside advanced mode at `now`, in seconds
/// since 1970: not once ended, nor failed over a day ago.
func shown(contract: Contract, now: Int64) -> Bool {
    switch contract.state {
    case .closed: return false
    case .failed: return now - contract.createdAt < 86_400
    default: return true
    }
}

/// What a swap provider's or a gateway's fee rising since its quote means:
/// nothing was paid, and it is the owner's to accept at the new price.
func feeRose(agreed: UInt64, asked: UInt64) -> String {
    "The fee went up from \(sat(amount: agreed)) to \(sat(amount: asked)) since it was quoted. Nothing was paid: " +
        "see the new price to accept it."
}

/// `amount` sat in bitcoin, as a payment link says it: `0.00033000`.
func btc(amount: UInt64) -> String {
    String(format: "%llu.%08llu", amount / 100_000_000, amount % 100_000_000)
}

/// What a payment that finds too little to pay with is told to do about it.
enum Lack {
    /// Protect what is on Bitcoin: a contract is offered for it.
    case offer

    /// Open what is on Bitcoin, held or on its way: it is not in the balance yet.
    case bitcoin

    /// Nothing is held at all: receive first.
    case nothing

    /// Not to be told apart: the reason says it.
    case other
}

/// Where a payment that failed sends its payer. A contract offered comes
/// first, as it always has, for `insufficient` or an empty balance, whatever
/// else went wrong. Then a balance too small (`insufficient`) with `bitcoin`
/// held or coming, notes held or not: adding it mends that. Otherwise only
/// `noHub` (no hub the code names holds the amount) with no notes at all is
/// told to look at bitcoin, or to receive: any other failure is said as it
/// is, and so is `noHub` with notes held elsewhere, which more bitcoin does
/// not mend.
func lack(insufficient: Bool, noHub: Bool, notes: UInt64, bitcoin: UInt64, offered: Bool) -> Lack {
    if offered && (insufficient || notes == 0) { return .offer }
    if insufficient && bitcoin > 0 { return .bitcoin }
    if notes != 0 || !(insufficient || noHub) { return .other }
    if bitcoin > 0 { return .bitcoin }
    return .nothing
}

/// Whether bitcoin held can be added to the balance now: a coin on Bitcoin,
/// none being added already, and none paying for a protection being set up
/// (`opening`). Otherwise it is on its way, and only waited for.
func addable(balance: Balance, opening: Bool) -> Bool {
    balance.onchain > 0 && balance.moving == 0 && !opening
}

/// What is on Bitcoin, paid to the wallet and not its own to keep, that is
/// not in the balance and that nothing is being done about: none offered
/// to protect it, no protection being set up, none of it being added. The
/// wallet's own change and payouts (`kept`) wait on purpose and are not
/// counted. Nil if there is none.
func waiting(balance: Balance, offered: Bool, opening: Bool) -> UInt64? {
    if offered || opening || balance.moving > 0 || balance.onchain <= balance.kept { return nil }
    return balance.onchain - balance.kept
}
