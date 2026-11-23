import LicashCore
import SwiftUI
import UIKit

/// How often an invoice shown is followed, in ms.
private let FOLLOW_MS: Int64 = 3_000

/// How long copying says it copied, in ms.
let COPIED_MS: Int64 = 2_000

/// What Receive shows: the way it picks for the amount, or one of the
/// others asked for, `label` naming it and `says` who pays it.
enum Way {
    /// A Lightning invoice a hub makes, or a provider where none does, for an amount: any wallet pays it.
    case lightning

    /// The wallet's code, paid in notes.
    case code

    /// The code on Bitcoin: an address any wallet pays, handed out once asked for, and the silent payment address.
    case bitcoin

    /// The silent payment address alone, for wallets paying those only.
    case silent

    /// A static LNURL of a swap provider's: any wallet pays it, again, while the phone is off.
    case donations

    var label: String { fields.label }
    var says: String { fields.says }
    var tag: String { fields.tag }

    private var fields: (label: String, says: String, tag: String) {
        switch self {
        case .lightning: return ("Lightning invoice", "Any Lightning wallet, for an amount", "way-lightning")
        case .code: return ("Licash code", "Licash wallets, cheaper and more private", "way-code")
        case .bitcoin: return ("Bitcoin address", "Any Bitcoin wallet, on Bitcoin", "way-bitcoin")
        case .silent: return ("Silent payment address", "Wallets paying silent payments, on Bitcoin", "way-silent")
        case .donations: return ("Donation address", "Any Lightning wallet, again and again, even while you're offline", "way-donations")
        }
    }
}

/// The way Receive picks for `amount`: an invoice any wallet pays, if a
/// hub trusted takes `lightning` payments, else the wallet's code.
func picked(amount: Int64?, lightning: Bool) -> Way {
    amount != nil && lightning ? .lightning : .code
}

/// The ways to be paid while a hub trusted takes `lightning` payments, and
/// `silent` ones, and while a provider takes `donations` to the wallet.
func ways(lightning: Bool, silent: Bool, donations: Bool = false) -> [Way] {
    [
        lightning ? Way.lightning : nil,
        Way.code,
        Way.bitcoin,
        silent ? Way.silent : nil,
        donations ? Way.donations : nil,
    ].compactMap { $0 }
}

/// The ways not open to the wallet now, each with why: said grayed out
/// under the others, so none is a mystery. `lookingForDonations` and
/// `lookingForLightning` while providers are still asked.
func closedWays(
    lightning: Bool,
    silent: Bool,
    donations: Bool,
    lookingForDonations: Bool,
    lookingForLightning: Bool = false
) -> [(Way, String)] {
    var closed: [(Way, String)] = []
    if !lightning {
        closed.append((.lightning, lookingForLightning ? "Looking for a provider…" : "None of your hubs takes Lightning"))
    }
    if !silent {
        closed.append((.silent, "Your hubs don't index silent payments"))
    }
    if !donations {
        closed.append((.donations, lookingForDonations ? "Looking for a provider…" : "No provider takes donations now"))
    }
    return closed
}

/// What a donation address takes and costs, and who pays it on, in words.
func donationWords(address: DonationAddress) -> String {
    var site = address.provider.range(of: "://").map { String(address.provider[$0.upperBound...]) } ?? address.provider
    while site.hasSuffix("/") { site.removeLast() }
    let by = address.name ?? site
    let fee = [
        address.feeBase > 0 ? sat(amount: address.feeBase) : nil,
        address.feePpm > 0 ? percent(ppm: address.feePpm) : nil,
    ].compactMap { $0 }.joined(separator: " + ")
    let paid = fee.isEmpty ? "Paid to you in notes by \(by), with no fee." : "Paid to you in notes by \(by), less its fee of \(fee)."
    if !address.reachable { return "\(paid) It isn't answering right now: payers may be refused until it does." }
    if address.stocked { return paid }
    return "\(paid) It can't take donations right now: payers are refused until it can."
}

/// The most characters of an invoice's memo, as the hub takes it.
let MAX_MEMO = 100

/// Whether a hub among `hubs` makes invoices saying a memo to their payer.
func memoTaken(hubs: [Hub]) -> Bool { hubs.contains { $0.lightning?.memo == true } }

/// What is typed as a memo: its words, one space apart, none if blank.
func memoOf(typed: String) -> String? {
    let words = typed.split(separator: #/\s+/#.asciiOnlyWhitespace()).filter { !$0.isEmpty }.joined(separator: " ")
    let memo = String(decoding: words.utf16.prefix(MAX_MEMO), as: UTF16.self)
    return memo.isEmpty ? nil : memo
}

/// What the payer of an invoice asked for an amount pays, and what the
/// wallet gets of it: in words, those differing from the amount only.
/// `by` made it: the hub, or a provider.
func invoiceWords(amount: UInt64, asks: UInt64, gets: UInt64, by: String = "the hub") -> String? {
    let words = [
        asks != amount ? "The payer pays \(sat(amount: asks))" : nil,
        gets != asks ? "You get \(sat(amount: gets)), \(by)'s fee taken" : nil,
    ].compactMap { $0 }.joined(separator: "\n")
    return words.isEmpty ? nil : words
}

/// What to be paid at, as a QR code and one Copy, picked for the payer: for
/// an amount, a Lightning invoice any wallet pays, from a hub taking
/// payments over Lightning. For any amount, the wallet's code, paid in
/// notes. The other ways, an address on Bitcoin any wallet pays among them,
/// one tap away in a sheet. Under it,
/// what it is and the amount asked, and nothing else.
struct Receive: View {
    let engine: Engine
    let back: () -> Void

    init(engine: Engine, back: @escaping () -> Void) {
        self.engine = engine
        self.back = back
    }

    @SwiftUI.State private var amount: Int64? = nil
    @SwiftUI.State private var code: Code? = nil
    @SwiftUI.State private var bitcoin: Code? = nil
    // Looked for once, out of sight: a way to be paid only if found.
    @SwiftUI.State private var donations: DonationAddress? = nil
    @SwiftUI.State private var lookingForDonations = true
    @SwiftUI.State private var error: String? = nil
    // The amount typed in, in place of the QR code: asked for once done.
    @SwiftUI.State private var editing = false
    @SwiftUI.State private var typed: Int64? = nil
    // Said to the payer of an invoice, at a hub that says one: typed with the amount.
    @SwiftUI.State private var memo: String? = nil
    @SwiftUI.State private var typedMemo = ""
    @SwiftUI.State private var received: String? = nil
    // Which code to be paid at: 0 the wallet's own, else an extra code's
    // number, separate from it, no one able to tell.
    @SwiftUI.State private var codeIndex = 0
    @SwiftUI.State private var codes: [ExtraCode] = []
    @SwiftUI.State private var pickingCode = false
    @SwiftUI.State private var naming = false
    @SwiftUI.State private var typedLabel = ""
    // A new identity being made: seconds, its hubs asked over Tor.
    @SwiftUI.State private var creating = false
    @SwiftUI.State private var createError: String? = nil
    @SwiftUI.State private var creatingWait = Wait()
    @SwiftUI.State private var copied = false
    @SwiftUI.State private var choosing = false
    // Where no hub takes Lightning, a provider's node may make the invoice:
    // looked for once, out of sight.
    @SwiftUI.State private var providerLightning = false
    @SwiftUI.State private var lookingForLightning = false
    // Picked for the amount, unless another way was asked for.
    @SwiftUI.State private var chosen: Way? = nil
    // An invoice, made by a hub: for an amount, paid once.
    @SwiftUI.State private var invoice: LightningInvoice? = nil
    @SwiftUI.State private var silentAddress: String? = nil
    @SwiftUI.State private var wait = Wait()

    private var ui: Ui { engine.ui }
    private var hubsLightning: Bool { ui.hubs.contains { $0.lightning != nil } }
    private var takesLightning: Bool { hubsLightning || providerLightning }
    private var invoicedBy: String { hubsLightning ? "the hub" : "the provider" }
    private var ways: [Way] {
        Licash.ways(lightning: takesLightning, silent: ui.hubs.contains { $0.silent }, donations: donations != nil)
    }

    private var way: Way {
        if let chosen, ways.contains(chosen) { return chosen }
        return picked(amount: amount, lightning: takesLightning)
    }

    private var lightning: Bool { way == .lightning }
    private var request: String? { lightning ? invoice?.request : nil }
    private var silent: String? { way == .silent ? silentAddress : nil }
    // No amount, as a silent payment address.
    private var anyAmount: Bool { way == .silent || way == .donations }

    private var shown: (String, String)? {
        switch way {
        case .lightning: return request.map { (way.label, $0) }
        case .silent: return silent.map { (way.label, $0) }
        case .donations: return donations.map { (way.label, $0.lnurl) }
        case .code: return code.map { (way.label, $0.text) }
        case .bitcoin: return bitcoin.map { (way.label, $0.text) }
        }
    }

    // Shown, its provider is kept: the same address shows from then on.
    private var shownDonations: String? { way == .donations ? donations?.lnurl : nil }

    // An invoice is what an amount asks for, unless another way was.
    private var invoicing: Bool {
        if let chosen, ways.contains(chosen) { return chosen == .lightning }
        return picked(amount: 1, lightning: takesLightning) == .lightning
    }

    private var memoed: String? { lightning ? memo : nil }

    private var pending: Bool {
        if editing || error != nil { return false }
        if lightning { return amount != nil && request == nil }
        if way == .donations { return donations == nil }
        if way == .silent { return silent == nil }
        return (way == .bitcoin ? bitcoin : code) == nil
    }

    private var asking: String {
        if way == .silent { return "Any amount, on Bitcoin" }
        if way == .donations { return donations.map { "\(sat(amount: $0.min)) to \(sat(amount: $0.max))" } ?? "Any amount" }
        return amount.map { sat(amount: UInt64($0)) } ?? (lightning ? "Choose an amount" : "Any amount")
    }

    // What the payer pays and the wallet gets, and what the hub is waiting for: nothing else.
    private var says: String? {
        let made = request != nil ? invoice : nil
        var paying: String? = nil
        if let made, let asks = made.asks, let amount {
            paying = invoiceWords(amount: UInt64(amount), asks: asks, gets: made.amount, by: invoicedBy)
        }
        let waits = lightning && amount != nil && request == nil ? invoice?.reason : nil
        switch way {
        case .silent: return "Found once in a block. Each payment lands at an address of its own."
        case .bitcoin: return "Found once in a block. Once paid, the next address shows. To pay with it, add it to your balance under Balance, On Bitcoin."
        case .donations: return donations.map { donationWords(address: $0) }
        case .lightning, .code: return paying ?? waits
        }
    }

    private func edit() {
        typed = amount
        typedMemo = memo ?? ""
        editing = true
    }

    private func done() {
        amount = typed
        memo = memoOf(typed: typedMemo)
        editing = false
    }

    private func create() {
        guard let label = labelOf(typed: typedLabel), !creating else { return }
        creating = true
        createError = nil
        Task {
            switch await engine.addCode(label: label) {
            case .success(let made):
                codes = codes + [made]
                codeIndex = Int(made.index)
                naming = false
            case .failure(let e):
                createError = explain(e: e)
            }
            creating = false
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            TopBar(title: "Receive", back: back)
            ScrollView {
                VStack(alignment: .center, spacing: 0) {
                    Spacer().frame(height: 16)
                    if let received {
                        Text(received)
                            .accessibilityIdentifier("received")
                            .textStyle(Theme.type.headlineSmall)
                            .foregroundStyle(Theme.color.primary)
                            .multilineTextAlignment(.center)
                        Spacer().frame(height: 16)
                    }
                    // In upper case, which a QR code holds in fewer modules.
                    if editing {
                        Spacer().frame(height: 32)
                        AmountField(amount: typed, onAmount: { typed = $0 }) { done() }
                        Spacer().frame(height: 16)
                        if invoicing && (memoTaken(hubs: ui.hubs) || !hubsLightning) {
                            MemoField(memo: typedMemo, by: invoicedBy, onMemo: { typedMemo = $0 }) { done() }
                        }
                    } else if lightning, let request {
                        QrCode(text: request.uppercased())
                    } else if lightning {
                        if amount != nil && error == nil { Waiting() }
                    } else if way == .donations {
                        if let donations { QrCode(text: donations.lnurl) }
                    } else if way == .silent {
                        if let silent {
                            QrCode(text: silent.uppercased())
                            Spacer().frame(height: 8)
                            Text(silent)
                                .accessibilityIdentifier("silent")
                                .textStyle(Theme.type.bodySmall)
                                .multilineTextAlignment(.center)
                        } else if error == nil {
                            Waiting()
                        }
                    } else if let made = way == .bitcoin ? bitcoin : code {
                        QrCode(text: made.qr)
                    } else if error == nil {
                        Waiting()
                    }
                    if !editing {
                        Waited(wait: wait, center: true).frame(maxWidth: .infinity).padding(.top, 8)
                    }
                    if let error {
                        Spacer().frame(height: 8)
                        Text(error).textStyle(Theme.type.bodyLarge).foregroundStyle(Theme.color.error).multilineTextAlignment(.center)
                    }
                    Spacer().frame(height: 16)
                    if !editing {
                        Text(way.label)
                            .accessibilityIdentifier("way")
                            .textStyle(Theme.type.labelLarge)
                            .foregroundStyle(Theme.color.onSurfaceVariant)
                        Spacer().frame(height: 4)
                        Text(asking)
                            .accessibilityIdentifier("asking")
                            .textStyle(Theme.type.titleMedium)
                        if let memoed {
                            Spacer().frame(height: 4)
                            Text("\u{201C}\(memoed)\u{201D}")
                                .accessibilityIdentifier("memo")
                                .textStyle(Theme.type.bodyMedium)
                                .multilineTextAlignment(.center)
                        }
                    }
                    if let says {
                        Text(says)
                            .textStyle(Theme.type.bodySmall)
                            .foregroundStyle(Theme.color.onSurfaceVariant)
                            .multilineTextAlignment(.center)
                    }
                    if !editing && way == .code {
                        TextButton(onClick: { pickingCode = true }) {
                            Text(codeName(index: UInt32(codeIndex), codes: codes))
                        }
                        .accessibilityIdentifier("which-code")
                    }
                    if !editing {
                        TextButton(onClick: { choosing = true }) {
                            Text("Other ways")
                        }
                        .accessibilityIdentifier("other-ways")
                    }
                }
                .frame(maxWidth: .infinity)
            }
            .frame(maxHeight: .infinity)
            HStack(alignment: .top, spacing: 12) {
                if editing {
                    BigButton(text: "Cancel", quiet: true) { editing = false }
                        .frame(maxWidth: .infinity)
                    BigButton(text: "Done") { done() }
                        .frame(maxWidth: .infinity)
                        .accessibilityIdentifier("done")
                } else {
                    // A silent payment address or a donation one asks for no amount.
                    if !anyAmount {
                        BigButton(text: "Amount", quiet: true, onClick: edit)
                            .frame(maxWidth: .infinity)
                            .accessibilityIdentifier("set-amount")
                    }
                    BigButton(text: copied ? "Copied" : "Copy", enabled: shown != nil) {
                        // The pasteboard takes no label: the text alone.
                        if case let (_, text)? = shown {
                            UIPasteboard.general.string = text
                            copied = true
                        }
                    }
                    .frame(maxWidth: .infinity)
                    .accessibilityIdentifier("copy")
                }
            }
            .padding(.vertical, 24)
        }
        .padding(.horizontal, 24)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .rememberWait(wait: wait, waiting: pending)
        .task(id: [AnyHashable(ui.hubs), AnyHashable(hubsLightning)]) {
            if hubsLightning { return }
            lookingForLightning = true
            let takes = (try? await engine.providerTakesLightning().get()) ?? false
            // Replaced by a newer run, this one's answer is stale.
            if Task.isCancelled { return }
            providerLightning = takes
            lookingForLightning = false
        }
        .task(id: way == .silent) {
            if way == .silent && silentAddress == nil {
                let made = await engine.silentAddress()
                if Task.isCancelled { return }
                switch made {
                case .success(let address): silentAddress = address
                case .failure(let e): error = explain(e: e)
                }
            }
        }
        .task(id: received) {
            let all = (try? await engine.codes().get()) ?? []
            if Task.isCancelled { return }
            codes = all
        }
        .task(id: [AnyHashable(amount), AnyHashable(received), AnyHashable(codeIndex)]) {
            code = nil
            error = nil
            let asked = amount.map { UInt64($0) }
            let made = codeIndex == 0
                ? await engine.code(amount: asked)
                : await engine.extraCode(index: UInt32(codeIndex), amount: asked)
            if Task.isCancelled { return }
            switch made {
            case .success(let made): code = made
            case .failure(let e): error = explain(e: e)
            }
        }
        // Only once asked for: an address handed out is watched from then on,
        // and made again once paid.
        .task(id: [AnyHashable(way == .bitcoin), AnyHashable(amount), AnyHashable(received)]) {
            bitcoin = nil
            if way != .bitcoin { return }
            error = nil
            let made = await engine.bitcoinCode(amount: amount.map { UInt64($0) })
            if Task.isCancelled { return }
            switch made {
            case .success(let made): bitcoin = made
            case .failure(let e): error = explain(e: e)
            }
        }
        .task(id: ui.hubs) {
            lookingForDonations = true
            let found = try? await engine.donationAddress().get()
            if Task.isCancelled { return }
            donations = found
            lookingForDonations = false
        }
        .task(id: shownDonations) {
            if let shownDonations { _ = await engine.keepDonationAddress(lnurl: shownDonations) }
        }
        .task(id: [AnyHashable(lightning), AnyHashable(amount), AnyHashable(memoed)]) {
            invoice = nil
            guard let asked = lightning ? amount : nil else { return }
            error = nil
            let made = await engine.askInvoiceFor(amount: UInt64(asked), memo: memoed)
            if Task.isCancelled { return }
            switch made {
            case .success(let made): invoice = made
            case .failure(let e): error = explain(e: e)
            }
        }
        // Followed while shown: its notes taken once it is paid.
        .task(id: invoice?.hash) {
            guard let hash = invoice?.hash else { return }
            while true {
                do { try await Task.sleep(for: .milliseconds(FOLLOW_MS)) } catch { return }
                guard let followed = try? await engine.followInvoice(hash: hash).get() else { continue }
                if Task.isCancelled { return }
                invoice = followed
                switch followed.state {
                case .done:
                    received = "Received \(sat(amount: followed.amount))"
                    amount = nil
                    memo = nil
                    return
                case .failed:
                    error = "Not paid: \(followed.reason ?? "canceled")"
                    return
                case .pending:
                    break
                }
            }
        }
        .task(id: copied) {
            if copied {
                do { try await Task.sleep(for: .milliseconds(COPIED_MS)) } catch { return }
                copied = false
            }
        }
        .onAppear {
            engine.eager = true
            engine.poke()
        }
        .onDisappear { engine.eager = false }
        .task {
            for await event in engine.events() {
                if event.received != nil { received = event.text }
            }
        }
        .sheet(isPresented: $pickingCode) {
            Sheet(title: "Which identity", dismiss: { pickingCode = false }) {
                Entry(title: MAIN_IDENTITY, sub: "The one your contacts know", tag: "code-0") {
                    pickingCode = false
                    codeIndex = 0
                }
                ForEach(codes, id: \.index) { extra in
                    Entry(title: extra.label, sub: codeWords(code: extra), tag: "code-\(extra.index)") {
                        pickingCode = false
                        codeIndex = Int(extra.index)
                    }
                }
                Entry(title: "New anonymous identity", sub: "No one can tell it's yours", tag: "new-code") {
                    pickingCode = false
                    typedLabel = ""
                    createError = nil
                    // Its wait counted afresh each time it opens.
                    creatingWait = Wait()
                    naming = true
                }
            }
        }
        .sheet(isPresented: $naming) {
            // Open while it is made, which asks its hubs: closed once made, or why not said under the button.
            Sheet(title: "New anonymous identity", dismiss: { naming = false }, dismissable: !creating) {
                OutlinedTextField(
                    value: typedLabel,
                    onValueChange: { typedLabel = String(decoding: $0.utf16.prefix(MAX_CODE_LABEL), as: UTF16.self) },
                    label: "Name, only you see it",
                    singleLine: true
                )
                .frame(maxWidth: .infinity)
                .accessibilityIdentifier("code-label")
                .textInputAutocapitalization(.sentences)
                .submitLabel(.done)
                .onSubmit { create() }
                Spacer().frame(height: 8)
                Text(SEPARATE_WORDS)
                    .textStyle(Theme.type.bodySmall)
                    .foregroundStyle(Theme.color.onSurfaceVariant)
                Spacer().frame(height: 16)
                BigButton(
                    text: creating ? "Creating…" : "Create",
                    enabled: labelOf(typed: typedLabel) != nil,
                    busy: creating
                ) { create() }
                .frame(maxWidth: .infinity)
                .accessibilityIdentifier("create-code")
                Waited(wait: creatingWait, center: true).frame(maxWidth: .infinity).padding(.top, 8)
                if let createError { Problem(text: createError) }
                Spacer().frame(height: 16)
            }
            .rememberWait(wait: creatingWait, waiting: creating)
        }
        .sheet(isPresented: $choosing) {
            Sheet(title: "Other ways", dismiss: { choosing = false }) {
                ForEach(ways.filter { $0 != way }, id: \.self) { other in
                    Entry(title: other.label, sub: other.says, tag: other.tag) {
                        choosing = false
                        chosen = other
                        error = nil
                        // An invoice asks for an amount.
                        if other == .lightning && amount == nil { edit() } else { editing = false }
                    }
                }
                let closed = closedWays(
                    lightning: takesLightning,
                    silent: ui.hubs.contains { $0.silent },
                    donations: donations != nil,
                    lookingForDonations: lookingForDonations,
                    lookingForLightning: lookingForLightning
                )
                ForEach(closed, id: \.0) { other, why in
                    Entry(
                        title: other.label,
                        sub: why,
                        tag: other.tag + "-closed",
                        color: Theme.color.onSurfaceVariant.opacity(0.6)
                    )
                }
            }
        }
    }
}

/// A memo for the invoice, typed under its amount: its payer reads it, and so
/// do `by`, the hub or provider making it, and its Lightning node, which the
/// words under it say.
private struct MemoField: View {
    let memo: String
    let by: String
    let onMemo: (String) -> Void
    let done: () -> Void

    init(memo: String, by: String, onMemo: @escaping (String) -> Void, done: @escaping () -> Void) {
        self.memo = memo
        self.by = by
        self.onMemo = onMemo
        self.done = done
    }

    var body: some View {
        OutlinedTextField(
            value: memo,
            onValueChange: { onMemo(String(decoding: $0.utf16.prefix(MAX_MEMO), as: UTF16.self)) },
            label: "Memo (optional)",
            singleLine: true
        )
        .frame(maxWidth: .infinity)
        .accessibilityIdentifier("memo-field")
        .textInputAutocapitalization(.sentences)
        .submitLabel(.done)
        .onSubmit { done() }
        Spacer().frame(height: 4)
        Text("The payer sees it, and so do \(by) and its Lightning node.")
            .textStyle(Theme.type.bodySmall)
            .foregroundStyle(Theme.color.onSurfaceVariant)
            .multilineTextAlignment(.center)
        Spacer().frame(height: 16)
    }
}

/// What a separate code is, said where one is made.
let SEPARATE_WORDS =
    "A code of its own to be paid at: no one can tell it's yours, or link it to your other identities. " +
    "Payments to it move to your balance, less a hub fee."

/// The wallet's own code, as an identity: the one its contacts know.
let MAIN_IDENTITY = "Main identity"

/// What an extra code took, in words.
func codeWords(code: ExtraCode) -> String {
    switch code.payments {
    case 0: return "Anonymous, nothing received yet"
    case 1: return "Anonymous, 1 payment received"
    default: return "Anonymous, \(code.payments) payments received"
    }
}

/// Where a QR code shows once made.
private struct Waiting: View {
    var body: some View {
        ZStack { CircularProgressIndicator() }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .aspectRatio(1, contentMode: .fit)
            .frame(maxWidth: 316)
    }
}
