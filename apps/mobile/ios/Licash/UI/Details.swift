import LicashCore
import SwiftUI
import UIKit

/// A line of the history on its own: what it cost and where it stands.
struct ActivityDetails: View {
    let engine: Engine
    let ui: Ui
    let shown: Activity
    let back: () -> Void
    @Environment(\.openURL) private var openURL
    @Environment(\.blockSeconds) private var blockSeconds
    @SwiftUI.State private var details: Details? = nil
    @SwiftUI.State private var error: String? = nil
    @SwiftUI.State private var unopened = false
    @SwiftUI.State private var notes: NotesOf? = nil
    // A contract's reserve: what of its price comes back.
    @SwiftUI.State private var reserve: UInt64? = nil

    // As it stands now: the history changes while shown.
    private var coming: Activity? { ui.incoming.first { $0.id == shown.id } }
    private var activity: Activity {
        coming ?? ui.history.first { $0.id == shown.id && $0.kind == shown.kind } ?? shown
    }

    var body: some View {
        let coming = self.coming
        let activity = self.activity
        // The pasteboard keeps no label: only the text.
        let copy: (String, String) -> Void = { _, text in UIPasteboard.general.string = text }
        let tx: Onchain? = switch details {
        case let .moved(_, _, _, _, payment, _, _, _, _): payment
        case let .coin(tx, _, _, _): tx
        default: nil
        }
        let payout: (hub: String, progress: PayoutSteps)? = switch details {
        case let .payout(hub, _, _, _, _, _, progress, _, _, _): (hub, progress)
        default: nil
        }
        let funding: String? = switch details {
        case let .contract(_, funding): funding
        default: nil
        }
        // A payout's claim pays the address: else the provider's coin, as soon
        // as the provider tells of it.
        let txid = tx?.txid ?? payout.flatMap { $0.progress.claim ?? $0.progress.coin } ?? funding
        let view: (String) -> Void = { link in
            unopened = false
            let failed = {
                AppLog.warn(line: "opening \(link)")
                unopened = true
            }
            guard let url = URL(string: link) else { return failed() }
            openURL(url) { opened in if !opened { failed() } }
        }
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                TopBar(title: title(activity: activity, coming: coming != nil), back: back)
                let sign = if activity.kind == .swap {
                    ""
                } else if paid(activity: activity) {
                    "−"
                } else {
                    "+"
                }
                Text(
                    activity.kind == .optimized ? optimizationCost(fee: activity.fee, state: activity.state)
                        : "\(sign)\(digits(amount: activity.amount)) sat"
                )
                .textStyle(Heading)
                .padding(.horizontal, 4)
                .padding(.vertical, 8)
                .accessibilityIdentifier("amount")
                Text(status(activity: activity, coming: coming != nil, details: details, blockSecs: blockSeconds))
                    .textStyle(Theme.type.bodyLarge)
                    .foregroundStyle(activity.state == .failed ? Theme.color.error : Theme.color.onSurfaceVariant)
                    .padding(.horizontal, 4)
                    .accessibilityIdentifier("state")
                // What it needs of its owner, never folded away.
                if let payout, let it = needs(activity: activity, p: payout.progress, developer: ui.developer, blockSecs: blockSeconds) {
                    Text(it)
                        .textStyle(Theme.type.bodyMedium)
                        .foregroundStyle(Theme.color.tertiary)
                        .padding(.horizontal, 4)
                        .padding(.vertical, 4)
                        .accessibilityIdentifier("needs")
                }
                if let error {
                    Text(error).textStyle(Theme.type.bodySmall).foregroundStyle(Theme.color.error)
                }
                Summary(
                    activity: activity,
                    details: details,
                    tx: tx,
                    developer: ui.developer,
                    view: txid.flatMap { engine.explorerLink(txid: $0) }.map { link in { view(link) } },
                    contact: ui.contacts[activity.key],
                    reserve: reserve
                )
                // Who paid, as they proved to this wallet alone: a contact, or one to keep.
                if activity.kind == .received, let from = activity.from {
                    SaveContact(engine: engine, code: from, payer: true)
                }
                if unopened {
                    Text("No app here opens web links.")
                        .textStyle(Theme.type.bodySmall)
                        .foregroundStyle(Theme.color.error)
                }
                // How its notes moved: for those who asked to see notes.
                let flow = ui.developer ? notes.flatMap { flowOf(notes: $0) } : nil
                if payout != nil || flow != nil || ui.developer {
                    Spacer().frame(height: 8)
                    More {
                        if let payout {
                            PayoutTimeline(
                                activity: activity,
                                hub: payout.hub,
                                progress: payout.progress,
                                developer: ui.developer,
                                link: { engine.explorerLink(txid: $0) },
                                open: view
                            )
                        }
                        if let flow {
                            Group(title: "") { NotesFlow(activity: activity, flow: flow) }
                        }
                        if ui.developer {
                            Technical(activity: activity, details: details, copy: copy)
                            if let notes, !notes.spent.isEmpty || !notes.made.isEmpty {
                                NotesGroup(activity: activity, notes: notes, copy: copy)
                            }
                        }
                    }
                    .accessibilityElement(children: .contain)
                    .accessibilityIdentifier("more")
                }
                Spacer().frame(height: 48)
            }
            .padding(.horizontal, 20)
        }
        .task(id: [AnyHashable(activity), AnyHashable(ui.scan?.next)]) {
            switch await engine.details(activity: activity) {
            case let .success(it):
                details = it
                error = nil
            case let .failure(e):
                error = explain(e: e)
            }
            if activity.kind == .contract {
                reserve = try? await engine.contractInspection(id: activity.id).get().holderReserve
            }
            switch await engine.notesOf(activity: activity) {
            case let .success(it): notes = it
            case let .failure(e): AppLog.warn(line: "its notes", e: e)
            }
        }
    }
}

func title(activity: Activity, coming: Bool) -> String {
    if coming && activity.kind != .closed {
        "Arriving"
    } else {
        switch activity.kind {
        case .sent: "Sent"
        case .received: "Received"
        case .moved: "From Bitcoin"
        case .funded: "From Bitcoin, protected"
        case .onchain: "On Bitcoin"
        case .lightning: "Paid over Lightning"
        case .invoice: "Received over Lightning"
        case .payout: "Sent on Bitcoin"
        case .contract: "Protection"
        case .swap: "Moved"
        case .optimized: "Optimized notes"
        case .closed: "Protection closed"
        }
    }
}

/// What her part of a coin a contract took is, said on its line.
let FUNDED_MEANS = "The rest of your coin on Bitcoin, after the cost and reserve of its " +
    "protection: added to your balance once it confirmed. The reserve comes back when it ends."

/// Of a contract's `price`, what it cost: the rest, its `reserve`, comes back.
func opened(price: UInt64, reserve: UInt64) -> UInt64 { price - min(price, reserve) }

/// Whether it paid out, rather than in.
func paid(activity: Activity) -> Bool {
    activity.kind == .sent || activity.kind == .lightning || activity.kind == .payout ||
        activity.kind == .contract || activity.kind == .optimized
}

/// What a wait for a block is, said the same way wherever one shows.
private let CONFIRMING = "Confirming, about 10 min"

/// Where it stands, in a few words: what a newcomer asks of a payment, not
/// how it is made. Never a block's height, nor who makes it happen.
func status(
    activity: Activity,
    coming: Bool,
    details: Details?,
    blockSecs: UInt32 = BITCOIN_BLOCK_SECONDS
) -> String {
    if coming { return CONFIRMING }
    if activity.state == .failed, case let .payout(_, _, _, _, _, _, progress, _, _, _) = details {
        switch progress.lock {
        case .locked: return "\(activity.detail). Your money comes back by itself."
        default: return "\(activity.detail). Nothing was taken."
        }
    }
    if activity.state == .failed, case let .swap(_, _, _, lock, _, _, _, _) = details {
        switch lock {
        case nil: return "\(activity.detail). Nothing was taken."
        default: return "\(activity.detail). Your money comes back by itself."
        }
    }
    if activity.state == .failed { return activity.detail }
    // A swap provider declined or failed it: its lock returns the sats once expired.
    if activity.state == .pending, case let .lightning(_, _, _, failure, provider, _, refundBlocks) = details,
       provider != nil, failure != nil {
        return notPaid(blocks: refundBlocks, blockSecs: blockSecs)
    }
    switch details {
    case let .payout(_, _, _, _, _, _, progress, _, _, _):
        if activity.state == .done { return "Arrived" }
        if progress.claim != nil { return "Sent. \(CONFIRMING)" }
        return "On its way"
    case .swap: return activity.state == .done ? "Done" : "Moving"
    case let .moved(_, _, _, _, payment, depth, _, _, _):
        return moving(activity: activity, payment: payment, depth: depth, blockSecs: blockSecs)
    case let .coin(_, _, _, moving): return moving != nil ? "Being added to your balance" : "Yours, on Bitcoin"
    case .invoice: return activity.state == .done ? "In your balance" : "Paid, arriving"
    case .contract: return activity.state == .done ? "Set up" : CONFIRMING
    default:
        if activity.state == .pending { return "Under way" }
        if paid(activity: activity) { return "Paid" }
        return "Received"
    }
}

/// Where bitcoin coming into the balance stands: the blocks its hub waits for, as time.
private func moving(activity: Activity, payment: Onchain?, depth: UInt32?, blockSecs: UInt32) -> String {
    if activity.state == .done { return "In your balance" }
    guard let paid = payment else { return "Waiting for its payment" }
    let depth = Int(depth ?? 1)
    let left = depth - Int(paid.confirmations)
    return left > 0 ? "Confirming, about \(minutes(blocks: Int64(left), blockSecs: blockSecs)) min" : "Almost there"
}

/// What a payout under way needs of its owner, if anything: the app open
/// before its time runs out, this phone finishing it. In advanced mode,
/// the block it runs out at too.
func needs(
    activity: Activity,
    p: PayoutSteps,
    developer: Bool,
    blockSecs: UInt32 = BITCOIN_BLOCK_SECONDS
) -> String? {
    guard let by = p.claimBy else { return nil }
    if activity.state != .pending || p.claim != nil { return nil }
    let block = developer ? " (block \(by))" : ""
    return if p.tip == 0 {
        "Keep the app open to finish it\(block)"
    } else if p.tip > by {
        "Too late to finish\(block): your money comes back by itself"
    } else {
        "Open the app within \(span(blocks: Int64(by - p.tip), blockSecs: blockSecs)) to finish it\(block)"
    }
}

/// The confirmations of a transaction in block `height`, at `tip`.
private func depthOf(height: UInt32?, tip: UInt32) -> Int {
    height.map { tip >= $0 ? Int(tip - $0) + 1 : 0 } ?? 0
}

/// Each step of a payout: its notes locked for the provider, the provider's
/// coin sent and confirmed, the coin claimed paying the address, and that
/// confirmed. Named plainly, how each is made told in advanced mode only.
/// A transaction known links to the block explorer.
private struct PayoutTimeline: View {
    let activity: Activity
    let hub: String
    let progress: PayoutSteps
    let developer: Bool
    let link: (String) -> String?
    let open: (String) -> Void

    // How it is made, step by step, is for advanced mode: else, that it is.
    private func said(detail: String?) -> String? { developer ? detail : nil }

    var body: some View {
        let p = progress
        // Read once, for the steps below.
        let coin = p.coin
        let coinHeight = p.coinHeight
        let claim = p.claim
        let claimHeight = p.claimHeight
        let failed = activity.state == .failed
        let done = activity.state == .done
        let view = { (txid: String?) -> (String, () -> Void)? in
            txid.flatMap(link).map { url in ("View", { open(url) }) }
        }
        let coinDepth = depthOf(height: coinHeight, tip: p.tip)
        let coinConfirmed = claim != nil || (coinHeight != nil && coinDepth >= Int(p.depth))
        let hub = host(url: self.hub)
        Group(title: "") {
            let (lockMark, lockDetail): (Mark, String) = switch p.lock {
            case nil: (failed ? .failed : .now, "Locking them at \(hub)")
            case .requested: (failed ? .failed : .now, "Waiting for \(hub) to certify the lock")
            case .locked: (.done, "At \(hub). The provider gets them only once you claim its coin; " +
                "else they come back to you after block \(p.lockExpiry.map { "\($0)" } ?? "null").")
            case .paid: (.done, "Paid to the provider at \(hub)")
            case .refunded: (.failed, "Given back to you at \(hub)")
            case .abandoned: (.failed, "Never made: your notes stayed yours")
            }
            Step(mark: lockMark, title: "Set aside", detail: said(detail: lockDetail), tag: "step locked")
            Step(
                mark: coin != nil ? .done
                    : !failed && p.lock == .locked ? .now
                    : .next,
                title: "Bitcoin ready",
                detail: said(
                    detail: coin.map { id in
                        "Transaction \(short(address: id))" + (p.timeout.map { ", yours to claim before block \($0)" } ?? "")
                    }
                ),
                action: view(coin),
                tag: "step coin"
            )
            Step(
                mark: coinConfirmed ? .done
                    : failed || coin == nil ? .next
                    : .now,
                title: "Confirmed",
                detail: coinConfirmed ? said(detail: coinHeight.map { "In block \($0)" })
                    : coinHeight != nil ? said(detail: "\(coinDepth) of \(p.depth) confirmations")
                    : coin != nil && !failed ? "About 10 min"
                    : nil,
                tag: "step coin-confirmed"
            )
            Step(
                mark: claim != nil ? .done
                    : failed || !coinConfirmed ? .next
                    : .now,
                title: "Sent to the address",
                detail: said(
                    detail: claim.map { "Transaction \(short(address: $0)), broadcast by this phone" }
                        ?? (coinConfirmed && !failed ? "Claiming it" : nil)
                ),
                action: view(claim),
                tag: "step claimed"
            )
            Step(
                mark: done ? .done
                    : failed || claim == nil ? .next
                    : .now,
                title: "Arrived",
                detail: done ? said(detail: claimHeight.map { "In block \($0)" })
                    : claimHeight != nil ? said(detail: "\(depthOf(height: claimHeight, tip: p.tip)) of \(p.depth) confirmations")
                    : claim != nil && !failed ? "About 10 min"
                    : nil,
                last: true,
                tag: "step arrived"
            )
        }
    }
}

// Nested, as the Summary of Stats holds the name in the app.
extension ActivityDetails {
    /// What it cost, where it went or came from, and when: its amount is above.
    /// Who made it happen is for advanced mode. `view` shows it on Bitcoin,
    /// if it is there.
    private struct Summary: View {
        let activity: Activity
        let details: Details?
        let tx: Onchain?
        let developer: Bool
        let view: (() -> Void)?
        var contact: String? = nil
        var reserve: UInt64? = nil

        private var failed: Bool { activity.state == .failed }

        // What it cost on top, and what it came to: nothing of one that failed.
        @ViewBuilder private func costs(breakdown: FeeBreakdown) -> some View {
            if !failed {
                FeeLine(label: "Fee", fee: activity.fee, amount: activity.amount, breakdown: breakdown)
                if activity.fee > 0 { Line(label: "Total", value: sat(amount: activity.amount + activity.fee), tag: "total") }
            }
        }

        var body: some View {
            Group(title: "") {
                // A contract's money back, a coin on Bitcoin: from where, not from whom.
                if activity.kind == .closed {
                    Line(label: "From", value: "Protection at \(name(url: activity.detail))", tag: "closed-from")
                    Line(label: "To", value: "Your Bitcoin", tag: "closed-to")
                } else {
                    switch details {
                    case let .moved(hub, _, _, _, payment, _, _, _, _):
                        FeeLine(
                            label: "Hub fee", fee: activity.fee, amount: activity.amount, tag: "fee-hub", value: sat(amount: activity.fee),
                            breakdown: recordedFee(amount: activity.amount, fee: activity.fee, components: [("Hub", activity.fee)])
                        )
                        if let it = payment?.fee {
                            FeeLine(
                                label: "Network fee", fee: it, amount: activity.amount, tag: "fee-network", value: sat(amount: it),
                                breakdown: FeeBreakdown(
                                    amount: activity.amount, fee: it, components: [("Bitcoin network", it)],
                                    explanation: "Fee of the full funding transaction. It is not a per-deposit allocation or a hub fee."
                                )
                            )
                        }
                        Line(label: "At", value: name(url: hub))
                    case let .coin(_, _, change, _):
                        Line(label: "From", value: change ? "Your own change" : "Bitcoin")
                    case let .lightning(hub, _, _, _, provider, _, _):
                        if failed {
                            FeeLine(
                                label: "Refund fee", fee: activity.fee, amount: activity.amount, value: sat(amount: activity.fee),
                                breakdown: recordedFee(amount: activity.amount, fee: activity.fee)
                            )
                        } else {
                            costs(breakdown: recordedFee(amount: activity.amount, fee: activity.fee))
                        }
                        Line(label: "From", value: name(url: hub))
                        if let provider { Line(label: "Through", value: host(url: provider), tag: "through") }
                    case let .payout(hub, provider, address, _, _, _, _, providerFee, claimFee, lockFee):
                        costs(
                            breakdown: payoutFee(
                                amount: activity.amount, fee: activity.fee, hub: lockFee, provider: providerFee, network: claimFee,
                                quoted: false
                            )
                        )
                        Line(label: "To", value: short(address: address), tag: "to")
                        Line(label: "From", value: name(url: hub))
                        if developer { Line(label: "Through", value: host(url: provider)) }
                    case let .invoice(hub, _, asks, _):
                        if let it = asks, it > activity.amount {
                            FeeLine(
                                label: "Payer's fee", fee: it - activity.amount, amount: activity.amount, value: sat(amount: it - activity.amount),
                                breakdown: recordedFee(amount: activity.amount, fee: it - activity.amount)
                            )
                        }
                        Line(label: "At", value: name(url: hub))
                    case let .contract(hub, _):
                        if let it = reserve {
                            Line(label: "Cost", value: sat(amount: opened(price: activity.amount, reserve: it)), tag: "contract-cost")
                            Line(label: "Reserve, refundable", value: sat(amount: it), tag: "contract-reserve")
                        }
                        Line(label: "Paid from", value: "Your Bitcoin", tag: "paid-from")
                        Line(label: "At", value: name(url: hub), tag: "at")
                    case let .swap(from, to, provider, _, _, _, providerFee, lockFee):
                        costs(breakdown: swapFee(amount: activity.amount, fee: activity.fee, hub: lockFee, provider: providerFee))
                        Line(label: "From", value: name(url: from))
                        Line(label: "To", value: name(url: to))
                        if developer { Line(label: "Through", value: host(url: provider)) }
                    default:
                        if activity.kind == .sent {
                            costs(breakdown: recordedFee(amount: activity.amount, fee: activity.fee))
                            if case let .notes(hub) = details { Line(label: "From", value: name(url: hub)) }
                        } else {
                            if case let .notes(hub) = details { Line(label: "At", value: name(url: hub)) }
                            if activity.kind == .funded { Say(text: FUNDED_MEANS) }
                            if activity.kind == .optimized {
                                Say(text: "Smaller notes leave room for receiving and help recovery fit its note limit.")
                            }
                        }
                    }
                }
                // Whom it was paid to: a contact. Who paid is said below, as proved.
                if activity.kind == .sent, let contact { Line(label: "To", value: contact, tag: "contact") }
                let noted = activity.kind == .sent || activity.kind == .received
                if let memo = activity.memo { Line(label: noted ? "Note" : "Memo", value: memo, tag: "memo") }
                if let code = activity.code {
                    Line(label: "To", value: code, tag: "code")
                    if activity.fee > 0 {
                        FeeLine(label: "Fee to move it to your balance", fee: activity.fee, amount: activity.amount)
                    }
                }
                if developer, let tx { Line(label: "Confirmations", value: confirmations(tx: tx, details: details)) }
                if let it = activity.at { Line(label: "Date", value: at(seconds: it), tag: "date") }
                if let view {
                    Divider()
                    Entry(title: "View on Bitcoin", onClick: view).accessibilityIdentifier("explorer")
                }
            }
        }
    }
}

/// A long address, its ends shown.
private func short(address: String) -> String {
    address.count <= 20 ? address : String(address.prefix(10)) + "…" + String(address.suffix(8))
}

private func confirmations(tx: Onchain, details: Details?) -> String {
    let depth: UInt32? = switch details {
    case let .moved(_, _, _, _, _, depth, _, _, _): depth
    default: nil
    }
    return if let depth, tx.confirmations < depth {
        "\(tx.confirmations) of \(depth)"
    } else {
        "\(tx.confirmations)"
    }
}

/// Everything known of it, in advanced mode: a tap copies a value.
private struct Technical: View {
    let activity: Activity
    let details: Details?
    let copy: (String, String) -> Void

    var body: some View {
        Group(title: "Advanced") {
            switch details {
            case let .notes(hub):
                Fact(label: "Payment", value: activity.id, copy: copy)
                Fact(label: "Hub", value: hub, copy: copy)
            case let .moved(hub, hash, pays, address, payment, depth, fundBy, timeout, refund):
                Fact(label: "Deposit hash", value: hash, copy: copy)
                Fact(label: "Hub", value: hub, copy: copy)
                if let it = address { Fact(label: "Address", value: it, copy: copy) }
                if let it = pays { Fact(label: "Pays", value: sat(amount: it), copy: copy, copied: "\(it)") }
                if let it = payment { Transaction(tx: it, copy: copy) }
                if let it = depth { Fact(label: "Confirmations needed", value: "\(it)", copy: copy) }
                if let it = fundBy { Fact(label: "Confirmed by block", value: "\(it)", copy: copy) }
                if let it = timeout { Fact(label: "Refundable after block", value: "\(it)", copy: copy) }
                if let it = refund { Fact(label: "Refund", value: it, copy: copy) }
            case let .coin(tx, vout, change, moving):
                Transaction(tx: tx, copy: copy)
                Fact(label: "Output", value: "\(vout)", copy: copy)
                if change { Fact(label: "Change", value: "of a payment of this wallet's", copy: copy) }
                if let it = moving { Fact(label: "Spent by", value: it, copy: copy) }
            case let .lightning(hub, invoice, preimage, failure, provider, expiry, _):
                Fact(label: "Lock", value: activity.id, copy: copy)
                Fact(label: "Hub", value: hub, copy: copy)
                Fact(label: "Invoice", value: invoice, copy: copy)
                if let it = preimage { Fact(label: "Preimage", value: it, copy: copy) }
                if let it = failure { Fact(label: "Failure", value: it, copy: copy) }
                if let it = provider { Fact(label: "Provider", value: it, copy: copy) }
                if let it = expiry { Fact(label: "Expiry", value: "\(it)", copy: copy) }
            case let .payout(hub, provider, address, _, _, failure, p, _, _, _):
                Fact(label: "Payout hash", value: activity.id, copy: copy)
                Fact(label: "Address", value: address, copy: copy)
                Fact(label: "Hub", value: hub, copy: copy)
                Fact(label: "Provider", value: provider, copy: copy)
                if let it = p.coin { Fact(label: "Provider's coin", value: it, copy: copy) }
                if let it = p.coinHeight { Fact(label: "Its block", value: "\(it)", copy: copy) }
                if let it = p.claim { Fact(label: "Claim", value: it, copy: copy) }
                if let it = p.claimHeight { Fact(label: "The claim's block", value: "\(it)", copy: copy) }
                if let it = p.lock { Fact(label: "Lock", value: "\(it)".lowercased(), copy: copy) }
                if let it = p.lockExpiry { Fact(label: "Lock refunded from block", value: "\(it)", copy: copy) }
                if let it = p.timeout { Fact(label: "Provider takes its coin back from block", value: "\(it)", copy: copy) }
                if let it = p.claimBy { Fact(label: "Claimed by block", value: "\(it)", copy: copy) }
                Fact(label: "Confirmations needed", value: "\(p.depth)", copy: copy)
                if let it = failure { Fact(label: "Failure", value: it, copy: copy) }
            case let .invoice(hub, invoice, _, failure):
                Fact(label: "Payment hash", value: activity.id, copy: copy)
                Fact(label: "Hub", value: hub, copy: copy)
                if let it = invoice { Fact(label: "Invoice", value: it, copy: copy) }
                if let it = failure { Fact(label: "Failure", value: it, copy: copy) }
            case let .contract(hub, funding):
                Fact(label: "Contract", value: activity.id, copy: copy)
                Fact(label: "Hub", value: hub, copy: copy)
                Fact(label: "Funding", value: funding, copy: copy)
            case let .swap(from, to, provider, lock, theirs, failure, _, _):
                Fact(label: "Swap hash", value: activity.id, copy: copy)
                Fact(label: "From", value: from, copy: copy)
                Fact(label: "To", value: to, copy: copy)
                Fact(label: "Provider", value: provider, copy: copy)
                if let it = lock { Fact(label: "Your lock", value: it, copy: copy) }
                if let it = theirs { Fact(label: "The provider's lock", value: it, copy: copy) }
                if let it = failure { Fact(label: "Failure", value: it, copy: copy) }
            case nil:
                Fact(label: "Id", value: activity.id, copy: copy)
            }
        }
    }
}

/// The notes in and out, in advanced mode: those it spent, whom they paid
/// and what it kept, and those it made the wallet's, each with its hub and
/// whether its hub can still tell which payment it came from. A tap copies
/// a note's identifier.
private struct NotesGroup: View {
    let activity: Activity
    let notes: NotesOf
    let copy: (String, String) -> Void

    var body: some View {
        Group(title: "Each note") {
            let made = notes.made
            let unlinked = made.filter { $0.linking == .unlinked }.count
            let said: [String?] = [
                notes.spent.isEmpty ? nil : "Spent \(count(notes: notes.spent)).",
                made.isEmpty ? nil : "Made \(count(notes: made))" + (unlinked > 0 ? ", \(unlinked) unlinked since." : "."),
            ]
            Text(
                said.compactMap { $0 }.joined(separator: " ") + " An unlinked note was signed again blindly by its hub: " +
                    "the hub can't tell which payment it came from when it is spent."
            )
            .textStyle(Theme.type.bodySmall)
            .foregroundStyle(Theme.color.onSurfaceVariant)
            .padding(.vertical, 8)
            if !notes.spent.isEmpty {
                Text("In: spent").textStyle(Theme.type.titleSmall).padding(.top, 8)
                ForEach(notes.spent, id: \.id) { NoteRow(note: $0, spentHere: true, copy: copy) }
            }
            if !notes.paid.isEmpty {
                Text("Out: paid and kept").textStyle(Theme.type.titleSmall).padding(.top, 8)
                if activity.state == .pending {
                    Text(
                        activity.kind == .sent ? "Once its hub releases it."
                            : "Once paid. Refunded instead, the notes come back less the hub's refund fee."
                    )
                    .textStyle(Theme.type.bodySmall)
                    .foregroundStyle(Theme.color.onSurfaceVariant)
                }
                ForEach(notes.paid, id: \.self) { PaidRow(kind: activity.kind, part: $0) }
            }
            if !made.isEmpty {
                Text(notes.paid.isEmpty ? "In: made the wallet's" : "Made the wallet's")
                    .textStyle(Theme.type.titleSmall)
                    .padding(.top, 8)
                ForEach(made, id: \.id) { NoteRow(note: $0, spentHere: false, copy: copy) }
            }
        }
    }
}

/// Part of what the notes spent paid: to whom, how much, in which notes.
private struct PaidRow: View {
    let kind: Kind
    let part: NotesPaid

    var body: some View {
        let to = paidTo(kind: kind, to: part.to)
        HStack(alignment: .center, spacing: 0) {
            VStack(alignment: .leading, spacing: 0) {
                Text(to).textStyle(Theme.type.bodyLarge)
                let each = part.notes.map { digits(amount: $0) }.joined(separator: " + ")
                let what = part.notes.count == 1 ? "1 note" : "\(part.notes.count) notes"
                let said: [String?] = [host(url: part.hub), each.isEmpty ? nil : "\(what): \(each)"]
                Text(said.compactMap { $0 }.joined(separator: " · "))
                    .textStyle(Theme.type.bodySmall)
                    .foregroundStyle(Theme.color.onSurfaceVariant)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            Spacer().frame(width: 12)
            Text(sat(amount: part.amount)).textStyle(Theme.type.bodyLarge)
        }
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// `3 notes, 4,096 sat`.
private func count(notes: [NoteLine]) -> String {
    let total = notes.reduce(UInt64(0)) { sum, n in sum + n.amount }
    let what = notes.count == 1 ? "1 note" : "\(notes.count) notes"
    return "\(what), \(sat(amount: total))"
}

/// A note: its amount, hub and linking, and where it went or came from.
private struct NoteRow: View {
    let note: NoteLine
    let spentHere: Bool
    let copy: (String, String) -> Void
    @SwiftUI.State private var done = false

    var body: some View {
        let linking = switch note.linking {
        case .unlinked: "unlinked"
        case .staysLinked: "stays linked: its hub refused to unlink it"
        case .linked: "linked, to be unlinked"
        }
        let `where`: String? = if note.gone {
            "spent by a copy of this wallet"
        } else if spentHere && note.madeByItself {
            "made for this payment's amounts"
        } else if !spentHere && note.spentBy != nil {
            "spent since"
        } else {
            nil
        }
        Button {
            copy("Note", note.id)
            done = true
        } label: {
            HStack(alignment: .center, spacing: 0) {
                VStack(alignment: .leading, spacing: 0) {
                    Text(sat(amount: note.amount)).textStyle(Theme.type.bodyLarge)
                    Text([host(url: note.hub), linking, `where`].compactMap { $0 }.joined(separator: " · "))
                        .textStyle(Theme.type.bodySmall)
                        .foregroundStyle(note.linking == .unlinked ? Theme.color.onSurfaceVariant : Theme.color.tertiary)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                Spacer().frame(width: 12)
                Text(done ? "Copied" : short(address: note.id))
                    .monospaced()
                    .textStyle(Theme.type.bodySmall)
                    .foregroundStyle(Theme.color.onSurfaceVariant)
            }
            .padding(.vertical, 8)
            .frame(maxWidth: .infinity, alignment: .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .task(id: done) {
            if done {
                do { try await Task.sleep(for: .milliseconds(COPIED_MS)) } catch { return }
                done = false
            }
        }
    }
}

private struct Transaction: View {
    let tx: Onchain
    let copy: (String, String) -> Void

    var body: some View {
        Fact(label: "Transaction", value: tx.txid, copy: copy)
        if let it = tx.height { Fact(label: "Block", value: "\(it)", copy: copy) }
        if let it = tx.fee { Fact(label: "Network fee", value: sat(amount: it), copy: copy, copied: "\(it)") }
    }
}

private struct Fact: View {
    let label: String
    let value: String
    let copy: (String, String) -> Void
    let copied: String
    @SwiftUI.State private var done = false

    init(label: String, value: String, copy: @escaping (String, String) -> Void, copied: String? = nil) {
        self.label = label
        self.value = value
        self.copy = copy
        self.copied = copied ?? value
    }

    var body: some View {
        Button {
            copy(label, copied)
            done = true
        } label: {
            VStack(alignment: .leading, spacing: 0) {
                Text(done ? "\(label) · Copied" : label)
                    .textStyle(Theme.type.labelMedium)
                    .foregroundStyle(Theme.color.onSurfaceVariant)
                Text(value).monospaced().textStyle(Theme.type.bodyMedium)
            }
            .padding(.vertical, 10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .task(id: done) {
            if done {
                do { try await Task.sleep(for: .milliseconds(COPIED_MS)) } catch { return }
                done = false
            }
        }
    }
}
