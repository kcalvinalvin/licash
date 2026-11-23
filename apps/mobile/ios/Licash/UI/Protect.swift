import LicashCore
import SwiftUI

/// A protection at a hub, in a row: how much it covers, how long it has
/// left and where it stands, and what it is worth, never folded away. A
/// tap unfolds the rest. Once forced closed (`recovery`), where that
/// stands instead. In advanced mode, forcing it closed.
struct CoverRow: View {
    let c: Contract
    let height: Int
    let network: Network
    let developer: Bool
    let recovery: Recovery?
    let end: () -> Void
    let forceClose: () -> Void
    let giveUp: () -> Void
    let allowFee: () -> Void
    let safety: () async -> ContractSafety?
    let inspection: () async -> ContractInspection?
    let explorer: (String) -> String?
    @Environment(\.blockSeconds) private var blockSecs
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @SwiftUI.State private var open = false
    @SwiftUI.State private var plan: ContractSafety? = nil
    @SwiftUI.State private var details: ContractInspection? = nil

    init(
        c: Contract,
        height: Int,
        network: Network,
        developer: Bool,
        recovery: Recovery?,
        end: @escaping () -> Void,
        forceClose: @escaping () -> Void,
        giveUp: @escaping () -> Void,
        allowFee: @escaping () -> Void = {},
        safety: @escaping () async -> ContractSafety? = { nil },
        inspection: @escaping () async -> ContractInspection? = { nil },
        explorer: @escaping (String) -> String? = { _ in nil }
    ) {
        self.c = c
        self.height = height
        self.network = network
        self.developer = developer
        self.recovery = recovery
        self.end = end
        self.forceClose = forceClose
        self.giveUp = giveUp
        self.allowFee = allowFee
        self.safety = safety
        self.inspection = inspection
        self.explorer = explorer
    }

    var body: some View {
        let colors = Theme.color
        let cover = cover(contract: c, height: height, blockSecs: blockSecs)
        let forcing = recovery.map { Licash.forcing(recovery: $0, height: height, blockSecs: blockSecs) }
        // While it is set up: the step it is at.
        let step = c.step.map { setup(step: $0) }
        let tint = if let forcing {
            forcing.failed ? colors.error : colors.tertiary
        } else if cover.failed {
            colors.error
        } else if cover.on {
            colors.primary
        } else {
            colors.onSurfaceVariant
        }
        VStack(alignment: .leading, spacing: 0) {
            Button {
                open.toggle()
            } label: {
                HStack(alignment: .center, spacing: 0) {
                    (cover.on ? Glyphs.shield : Glyphs.shieldOff)
                        .resizable()
                        .scaledToFit()
                        .frame(width: 20, height: 20)
                        .foregroundStyle(tint)
                        .accessibilityHidden(true)
                    Spacer().frame(width: 12)
                    VStack(alignment: .leading, spacing: 0) {
                        Text("Up to \(sat(amount: c.cap))")
                            .textStyle(Theme.type.bodyLarge)
                            .lineLimit(1)
                        if let says = forcing?.says ?? step?.1 ?? cover.left {
                            Text(says)
                                .textStyle(Theme.type.bodySmall)
                                .foregroundStyle(colors.onSurfaceVariant)
                                .accessibilityIdentifier(step != nil ? "contract-step" : "")
                        }
                        // How fast what it sends and gets goes, while it does.
                        if forcing == nil && step != nil, let transfer = c.transfer {
                            let preparing = if case .preparing? = c.step { true } else { false }
                            Text(transferred(t: transfer, busy: preparing))
                                .textStyle(Theme.type.bodySmall)
                                .foregroundStyle(colors.onSurfaceVariant)
                                .accessibilityIdentifier("contract-transfer")
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    Text(forcing?.stands ?? cover.stands)
                        .textStyle(Theme.type.labelLarge)
                        .foregroundStyle(tint)
                        .accessibilityIdentifier("contract-state")
                    Image(systemName: open ? "chevron.up" : "chevron.down")
                        .foregroundStyle(colors.onSurfaceVariant)
                        .frame(width: 24, height: 24)
                        .accessibilityLabel(open ? "Hide" : "Show")
                }
                .padding(.vertical, 8)
                .frame(maxWidth: .infinity, minHeight: 56, alignment: .leading)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            // What it is worth, in sight while it runs: nobody is to count on what protects nothing.
            if let step {
                LinearProgressIndicator(progress: step.0 / SETUP_STEPS)
                    .frame(maxWidth: .infinity)
                    .padding(.bottom, 8)
            }
            if cover.runs && recovery == nil { Worth(mock: c.mock) }
            if open {
                VStack(alignment: .leading, spacing: 0) {
                    if let recovery {
                        Forced(r: recovery, developer: developer, giveUp: giveUp, allowFee: allowFee, height: height)
                    } else {
                        Unfolded(c: c, cover: cover, developer: developer, end: end, forceClose: forceClose, safety: plan, height: height, network: network, reserve: details?.holderReserve)
                    }
                    if let details { ContractLayout(info: details, explorer: explorer) }
                }
                .transition(.opacity)
            }
        }
        .animation(reduceMotion ? nil : .default, value: open)
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("contract")
        .onChange(of: c.id) {
            open = false
            plan = nil
            details = nil
        }
        .task(id: [AnyHashable(open), AnyHashable(c.id), AnyHashable(c.state)]) {
            if open {
                if recovery == nil {
                    let it = await safety()
                    if Task.isCancelled { return }
                    plan = it
                }
                let it = await inspection()
                if !Task.isCancelled { details = it }
            }
        }
    }
}

/// One lock, split into its payout pool and reserves. The claim cap is a
/// limit inside the pool, never another slice of the same bitcoin.
private struct ContractLayout: View {
    let info: ContractInspection
    let explorer: (String) -> String?
    @Environment(\.openURL) private var openURL
    @SwiftUI.State private var technical = false

    var body: some View {
        let colors = Theme.color
        VStack(alignment: .leading, spacing: 0) {
            Line(label: "Locked on Bitcoin", value: sat(amount: info.locked), tag: "contract-locked")
            GeometryReader { g in
                let parts = [(info.payoutPool, colors.primary), (info.holderReserve, colors.tertiary), (info.hubReserve, colors.secondary)]
                    .filter { $0.0 > 0 }
                // Each its share of them all, as a row's weights are.
                let total = parts.reduce(0.0) { $0 + Double($1.0) }
                HStack(spacing: 0) {
                    ForEach(parts.indices, id: \.self) { i in
                        parts[i].1.frame(width: g.size.width * Double(parts[i].0) / total)
                    }
                }
            }
            .frame(height: 12)
            .frame(maxWidth: .infinity)
            .accessibilityIdentifier("contract-split")
            Line(label: "Payout pool", value: sat(amount: info.payoutPool))
            Line(label: "Your reserve", value: sat(amount: info.holderReserve))
            Line(label: "Hub reserve", value: sat(amount: info.hubReserve))
            Line(label: "Maximum claim", value: sat(amount: UInt64(info.claimLimit)))
            Say(text: "The claim limit is inside the pool, not extra bitcoin. The recovery outcome decides how the pool and reserves are paid.")
            if let link = explorer(info.address) {
                Entry(title: "View contract on Bitcoin", tag: "contract-explorer", onClick: {
                    if let url = URL(string: link) { openURL(url) }
                })
            }
            TextButton(onClick: { technical.toggle() }) {
                Text(technical ? "Hide technical details" : "Technical details")
            }
            .accessibilityIdentifier("contract-technical")
            if technical {
                Line(label: "Type", value: "Native corrected")
                Line(label: "Graph version", value: digits(amount: UInt64(info.graphVersion)))
                Line(label: "Claim note limit", value: digits(amount: UInt64(info.noteLimit)))
                Line(label: "Denominations (sat)", value: info.denominations.map { digits(amount: $0) }.joined(separator: ", "))
                Line(label: "Keyset", value: info.keyset)
                Line(label: "Hub program", value: info.program)
                Text(info.address).textStyle(Theme.type.bodySmall)
            }
        }
        .onChange(of: info.address) { technical = false }
    }
}

/// A protection's row unfolded: what it covers and costs. Closing it with its hub, forcing it closed in advanced mode.
private struct Unfolded: View {
    let c: Contract
    let cover: Cover
    let developer: Bool
    let end: () -> Void
    let forceClose: () -> Void
    let safety: ContractSafety?
    let height: Int
    let network: Network
    // Her reserve, of its price, once its layout is read: apart, as it comes back.
    var reserve: UInt64? = nil
    @SwiftUI.State private var planning = false

    var body: some View {
        let state = c.state
        if case .failed(let reason) = state { Line(label: "Why", value: reason) }
        // Of what its hub holds, those ending first covering first.
        if cover.on { Line(label: "Covers now", value: sat(amount: c.covers), tag: "covers") }
        if let ends = cover.ends { Line(label: "Ends by itself in", value: ends) }
        if !cover.failed {
            // Until its layout is read, which takes a while, what was paid, the reserve in it.
            let known = reserve.flatMap { $0 <= c.price ? $0 : nil }
            switch known {
            case nil:
                Line(label: "Paid", value: sat(amount: c.price), tag: "contract-paid")
            case let known?:
                let cost = c.price - known
                FeeLine(label: "Cost", fee: cost, amount: c.cap, value: sat(amount: cost), breakdown: setupCost(amount: c.cap, cost: cost))
                if known > 0 { Line(label: "Reserve, refundable", value: sat(amount: known), tag: "contract-reserve") }
            }
        }
        // What she funded of it herself: hers once issued, else its recovery claims it.
        if c.funded > 0 && !cover.failed {
            Line(
                label: "You funded",
                value: "\(sat(amount: c.funded)) · \(c.fundedReceived ? "in your balance" : "on its way")",
                tag: "contract-funded"
            )
        }
        if cover.held {
            // Its status at a glance. The margin and what it means, when tapped.
            if let safety {
                Line(label: "Recovery planning", value: safetyStatus(safety: safety, height: height), tag: "planning", onClick: { planning = true })
                    .sheet(isPresented: $planning) {
                        Sheet(title: "Recovery planning", dismiss: { planning = false }) {
                            Line(label: "Your exit margin", value: "\(safety.exitMargin) blocks")
                            Line(
                                label: "During a forced close",
                                value: "Come online at least every \(checkInHours(blocks: safety.challengeWindow, blockSecs: blockSeconds(network: network))) hours"
                            )
                            Say(text: "Planning only. Start earlier for confirmation time. Stay online: the wallet checks the hub's proof and must challenge a bad one within \(safety.challengeWindow) blocks.")
                        }
                    }
            }
        }
        if developer {
            Line(label: "Paid from", value: "A coin of \(sat(amount: c.paidFrom))")
            Line(label: "Protects until", value: "block \(c.protectsUntil)")
            Line(label: "Closes at", value: "block \(c.closesAt)")
            if let funding = c.funding { Line(label: "Funding", value: String(funding.prefix(16)) + "…") }
            Line(label: "Id", value: String(c.id.prefix(16)) + "…")
        }
        if cover.held {
            HStack(alignment: .top, spacing: 0) {
                TextButton(onClick: end) { Text("Close") }
                    .accessibilityIdentifier("close-contract")
                // Its hub silent: what it holds claimed on Bitcoin, unilaterally.
                if developer && cover.on {
                    TextButton(onClick: forceClose) {
                        Text("Force close").foregroundStyle(Theme.color.error)
                    }
                    .accessibilityIdentifier("force-close")
                }
            }
        }
    }
}

/// A protection forced closed, unfolded: what it claimed, what stopped it,
/// giving it up while it may be, and allowing its claim a higher fee once
/// the reserve's falls short.
private struct Forced: View {
    let r: Recovery
    let developer: Bool
    let giveUp: () -> Void
    let allowFee: () -> Void
    let height: Int

    var body: some View {
        Line(label: "Claimed", value: sat(amount: r.claimed), tag: "recovery-claimed")
        if let corrected = r.corrected {
            Line(label: "Owed, the hub says", value: sat(amount: corrected), tag: "recovery-corrected")
            Say(text: "Before reserves and Bitcoin fees.")
        }
        if let received = r.received { Line(label: "Back on Bitcoin", value: sat(amount: received), tag: "recovery-received") }
        if r.stage == .answered && height > 0, let deadline = r.deadline {
            Line(label: "Hub's time left to prove", value: "\(max(Int64(deadline) - Int64(height), 0)) blocks")
        }
        if r.feeCapped {
            Line(label: "Stopped by", value: "The fee its reserve pays", tag: "recovery-fee-capped")
            if developer, let error = r.error { Say(text: error) }
        } else if let error = r.error {
            Line(label: "Stopped by", value: error)
        }
        if developer {
            if let nextAt = r.nextAt { Line(label: "Next step at", value: "block \(nextAt)") }
            if let request = r.request { Line(label: "Request", value: String(request.prefix(16)) + "…") }
            if let publication = r.publication { Line(label: "Claim", value: String(publication.prefix(16)) + "…") }
            if let timeout = r.timeout { Line(label: "Payback", value: String(timeout.prefix(16)) + "…") }
            if let response = r.response { Line(label: "Hub's answer", value: String(response.prefix(16)) + "…") }
            if let deadline = r.deadline { Line(label: "Hub proves by", value: "block \(deadline)") }
            if let assertion = r.assertion { Line(label: "Hub's proof", value: String(assertion.prefix(16)) + "…") }
            if let payout = r.payout { Line(label: "Payout", value: String(payout.prefix(16)) + "…") }
            if let sweep = r.sweep { Line(label: "Sweep", value: String(sweep.prefix(16)) + "…") }
            if let fold = r.fold { Line(label: "Fold", value: String(fold.prefix(16)) + "…") }
        }
        if r.feeCapped {
            TextButton(onClick: allowFee) { Text("Allow a higher fee") }
                .accessibilityIdentifier("allow-publication-fee")
        }
        if forcing(recovery: r, height: height).revocable {
            TextButton(onClick: giveUp) { Text("Cancel closing") }
                .accessibilityIdentifier("give-up-recovery")
        }
    }
}

/// Let a forced close's claim pay up to a total fee she types, past what
/// its reserve pays, the rest from her coins on Bitcoin. Her claim must be
/// in a block within its window, about an hour, or what it claimed is lost.
struct AllowFeeSheet: View {
    let recovery: Recovery?
    let busy: Bool
    let allow: (UInt64) -> Void
    let dismiss: () -> Void
    @SwiftUI.State private var amount: Int64? = nil

    init(recovery: Recovery?, busy: Bool, allow: @escaping (UInt64) -> Void, dismiss: @escaping () -> Void) {
        self.recovery = recovery
        self.busy = busy
        self.allow = allow
        self.dismiss = dismiss
    }

    var body: some View {
        Sheet(title: "Allow a higher fee", dismiss: dismiss) {
            if recovery?.feeCapped == true {
                Say(
                    text: "Fees on Bitcoin are higher than this contract's reserve pays for publishing its claim. " +
                        "Allow a total fee for it: what the reserve does not cover comes from your coins on Bitcoin."
                )
                Say(text: "It must be in a block soon, within about an hour, or the close fails and the notes it claimed are lost.")
                AmountField(amount: amount, onAmount: { amount = $0 })
                if busy { Say(text: "Waiting for the contract sync under way to finish first.") }
                BigButton(
                    text: busy ? "Allowing…" : "Allow",
                    enabled: !busy && amount != nil,
                    onClick: { if let amount { allow(UInt64(clamping: amount)) } }
                )
                .frame(maxWidth: .infinity)
                .accessibilityIdentifier("allow-publication-fee-confirm")
                TextButton(onClick: dismiss, enabled: !busy) { Text("Not now") }
                    .frame(maxWidth: .infinity)
            } else {
                Say(text: "Its claim no longer waits for a higher fee.")
                BigButton(text: "Done", onClick: dismiss)
                    .frame(maxWidth: .infinity)
            }
        }
    }
}

/// Cancel only this unsigned attempt, not the contract or the right to try again.
struct GiveUpSheet: View {
    let recovery: Recovery?
    let busy: Bool
    let confirm: () -> Void
    let dismiss: () -> Void

    var body: some View {
        let allowed = recovery.map { forcing(recovery: $0, height: 0).revocable } == true
        Sheet(title: allowed ? "Cancel closing?" : "Closing changed", dismiss: dismiss) {
            if allowed, let recovery {
                Line(label: "Back to wallet", value: sat(amount: recovery.claimed))
                Line(label: "Notes", value: digits(amount: UInt64(recovery.notes)))
                Say(text: "Stops this attempt and releases its reserved notes. It does not close the contract or recover Bitcoin.")
                Say(text: "You can force close again while the contract allows it. An offline hub still cannot pay you.")
                if busy { Say(text: "Waiting for the contract sync under way to finish first: over Tor, this can take a few minutes.") }
                BigButton(text: busy ? "Canceling…" : "Stop this attempt", enabled: !busy, danger: true, onClick: confirm)
                    .frame(maxWidth: .infinity)
                    .accessibilityIdentifier("give-up-recovery-confirm")
                TextButton(onClick: dismiss, enabled: !busy) { Text("Keep closing") }
                    .frame(maxWidth: .infinity)
            } else {
                Say(text: "This attempt has moved on. It can no longer be canceled here.")
                BigButton(text: "Done", onClick: dismiss)
                    .frame(maxWidth: .infinity)
            }
        }
    }
}

/// What forcing a protection closed does, before it is done: for a hub that
/// stopped answering only, and final once its request is signed. With the
/// `shortfall` of her coins for a challenge's fee, if she holds none.
struct ForceSheet: View {
    let c: Contract
    let held: UInt64
    let shortfall: UInt64?
    let busy: Bool
    let error: String?
    let wait: Wait
    let force: () -> Void
    let dismiss: () -> Void

    var body: some View {
        let claimed = held < c.cap ? held : c.cap
        Sheet(title: "Force close", dismiss: dismiss) {
            Say(
                text: "Only if \(name(url: c.hub)) stopped answering: while it answers, close with it instead, which pays the same. " +
                    "Your \(sat(amount: claimed)) there leave your balance and are claimed on Bitcoin, " +
                    "back on Bitcoin in a few days if the hub stays silent. Its fees come from your reserve in the contract."
            )
            if let shortfall {
                Say(
                    text: "Keep a coin of at least \(sat(amount: shortfall)) on Bitcoin besides your others first: should the hub " +
                        "prove falsely, your challenge's fee comes from it, set aside until the close is over. " +
                        "Without it, notes of yours are cashed out for it while you are online, " +
                        "else a challenge pays what your coins hold, and may come too late."
                )
            }
            Say(text: "Once its request is signed, it cannot be canceled, even before it is on Bitcoin.")
            BigButton(text: busy ? "Forcing closed…" : "Force close", danger: true, busy: busy) { force() }
                .frame(maxWidth: .infinity)
                .accessibilityIdentifier("force-close-confirm")
            Waited(wait: wait)
            if let error { Problem(text: error) }
        }
    }
}

/// That a protection is worth nothing, under the mock authority, said where
/// one shows and before one is paid for. A genuine one this app recovers
/// from by itself: the hub proves, the holder only checks.
struct Worth: View {
    let mock: Bool

    var body: some View {
        if mock {
            Text(MOCK)
                .textStyle(Theme.type.bodySmall)
                .foregroundStyle(Theme.color.error)
                .padding(.bottom, 8)
                .accessibilityIdentifier("worth")
        }
    }
}

/// What a hub of one contract asks of the coin that pays for it besides its
/// price, when the coin is cashed out to pay it: her part of the contract,
/// which the hub issues back to her as notes.
let FIXED_PART: UInt64 = 1_000

/// How long opening a contract takes: a tester's took under four minutes, then its funding confirms.
let SETUP_TAKES = "A few minutes, then a confirmation on Bitcoin"

/// `bytes` in B, kB, MB and on by 1000s, the largest unit it is over 900
/// of: whole in bytes and from 10, else to a tenth, or a hundredth below 1:
/// `500 kB`, `0.95 MB`, `2.3 MB`, `23 MB`.
func shortFileSize(bytes: UInt64) -> String {
    let units = ["B", "kB", "MB", "GB", "TB", "PB"]
    // In single precision, halves rounded up: 1,050,000 bytes are 1.0 MB, 12,500,000 are 13 MB.
    var value = Float(bytes)
    var unit = 0
    while value > 900 && unit < units.count - 1 {
        value /= 1000
        unit += 1
    }
    let digits = unit == 0 || value >= 10 ? 0 : value >= 1 ? 1 : 2
    let scale = [1.0, 10, 100][digits]
    return String(format: "%.\(digits)f \(units[unit])", (Double(value) * scale).rounded(.toNearestOrAwayFromZero) / scale)
}

/// What protecting at a hub covers, lasts and costs, before it is paid:
/// to pay it, or to get a coin on Bitcoin that pays it first. A coin is spent
/// whole: it pays the price, and the rest of it is her part of the contract.
struct ProtectSheet: View {
    let q: ContractQuote
    let hub: String
    let open: () -> Void
    let cashOutFirst: () -> Void
    let dismiss: () -> Void
    @Environment(\.blockSeconds) private var blockSecs

    init(q: ContractQuote, hub: String, open: @escaping () -> Void, cashOutFirst: @escaping () -> Void, dismiss: @escaping () -> Void) {
        self.q = q
        self.hub = hub
        self.open = open
        self.cashOutFirst = cashOutFirst
        self.dismiss = dismiss
    }

    var body: some View {
        let download = shortFileSize(bytes: q.download)
        Sheet(title: "Protect at \(hub)", dismiss: dismiss) {
            Line(label: "Covers up to", value: sat(amount: q.cap), tag: "quote-cap")
            Line(label: "For", value: span(blocks: Int64(q.protectsFor), blockSecs: blockSecs), tag: "quote-for")
            FeeLine(label: "Cost", fee: q.price, amount: q.cap, tag: "quote-price", value: sat(amount: q.price), breakdown: setupCost(amount: q.cap, cost: q.price))
            Line(label: "Takes", value: "\(SETUP_TAKES) · \(download)")
            Worth(mock: q.mock)
            Spacer().frame(height: 8)
            if let coin = q.coin {
                // The coin is spent whole: the price, and the rest is hers, issued back as notes.
                let part = coin > q.price ? coin - q.price : 0
                Line(label: "Paid with", value: "\(sat(amount: coin)) on Bitcoin", tag: "contract-paid-with")
                Line(label: "Into your balance", value: sat(amount: part), tag: "contract-funded")
                BigButton(text: "Protect for \(sat(amount: q.price))") { open() }
                    .frame(maxWidth: .infinity)
                    .accessibilityIdentifier("protect")
            } else {
                Say(text: "Paid from one coin on Bitcoin you hold, spent whole, and none you hold pays for it. Cash out to yourself first, then protect.")
                BigButton(text: "Cash out \(sat(amount: q.price + FIXED_PART))") { cashOutFirst() }
                    .frame(maxWidth: .infinity)
                    .accessibilityIdentifier("cash-out-first")
            }
        }
    }
}

/// Closing a protection early with its hub: the hub folds it with you, and
/// what you hold there comes to you on Bitcoin, up to what it covers, with
/// your reserve. Never without what it covers: a protection that ends
/// leaves nothing unclaimed that it could have paid. What is held beyond
/// its cap stays at the hub, unprotected, and cashing it out first is what
/// takes it too.
struct EndSheet: View {
    let c: Contract
    let height: Int
    let held: UInt64
    let busy: Bool
    let error: String?
    let wait: Wait
    let cashOut: () -> Void
    let close: () -> Void
    let dismiss: () -> Void
    @Environment(\.blockSeconds) private var blockSecs

    init(
        c: Contract,
        height: Int,
        held: UInt64,
        busy: Bool,
        error: String?,
        wait: Wait,
        cashOut: @escaping () -> Void,
        close: @escaping () -> Void,
        dismiss: @escaping () -> Void
    ) {
        self.c = c
        self.height = height
        self.held = held
        self.busy = busy
        self.error = error
        self.wait = wait
        self.cashOut = cashOut
        self.close = close
        self.dismiss = dismiss
    }

    var body: some View {
        let ends = cover(contract: c, height: height, blockSecs: blockSecs).ends
        let claimed = held < c.cap ? held : c.cap
        Sheet(title: "Close with \(name(url: c.hub))", dismiss: dismiss) {
            if claimed > 0 {
                Say(
                    text: "\(name(url: c.hub)) pays you what you hold there, up to \(sat(amount: c.cap)), and your part of the protection's cost " +
                        "comes back with it: about \(sat(amount: claimed)) on Bitcoin, in one transaction. It protects nothing from then on."
                )
            } else {
                Say(
                    text: "You hold nothing at \(name(url: c.hub)): closing brings your part of the protection's cost back on Bitcoin, " +
                        "and it protects nothing from then on."
                )
            }
            if held > claimed {
                Say(
                    text: "\(sat(amount: held - claimed)) of what you hold there is beyond what it covers: it stays at \(name(url: c.hub)), " +
                        "unprotected. Cash out first to take it too."
                )
            }
            if let ends { Say(text: "Otherwise it ends by itself in \(ends).") }
            BigButton(text: busy ? "Closing…" : "Close and receive", busy: busy) { close() }
                .frame(maxWidth: .infinity)
                .accessibilityIdentifier("end-close")
            if held > 0 {
                Spacer().frame(height: 12)
                BigButton(text: "Cash out \(sat(amount: held)) first", enabled: !busy, quiet: true) { cashOut() }
                    .frame(maxWidth: .infinity)
                    .accessibilityIdentifier("end-cash-out")
            }
            Waited(wait: wait)
            if let error { Problem(text: error) }
        }
    }
}
