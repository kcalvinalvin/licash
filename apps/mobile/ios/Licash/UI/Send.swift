import LicashCore
import SwiftUI
import UIKit

/// Scan or paste: a code to pay, or a payment handed over to take; with
/// `cashOut`, a hub, a Bitcoin address to pay notes there out to, as much
/// as chosen: all of them, or `cashOutAmount` as suggested.
/// `ownAddress`, if `link` pays this wallet's own address, is shown so.
struct Send: View {
    let engine: Engine
    let ui: Ui
    let link: String?
    let cashOut: String?
    let ownAddress: String?
    let cashOutAmount: UInt64?
    let toReceive: (() -> Void)?
    let back: () -> Void

    // Inside Send, as the app's own `Step` is a view.
    private enum Step {
        case looking
        /// A code to pay in notes.
        case paying(code: String, asked: UInt64?, visit: Bool = false)
        case planning
        /// Swapping through `provider`, named `through`, if chosen.
        case review(code: String, amount: UInt64?, quote: PayQuote, provider: String? = nil, through: String? = nil)
        case visit(code: String, asked: UInt64?)
        case invoice(text: String, quote: InvoiceQuote)
        case address(address: String)
        case payout(quote: PayoutQuote, asked: UInt64? = nil)
        case busy
        case quoting
        case payingOut(amount: UInt64, depth: UInt32)
        /// To the code `code`, if one; `kept` at its hub for the payee to collect.
        case sent(amount: UInt64, handover: String?, code: String? = nil, kept: Bool = false)
        case paid(amount: UInt64, fee: UInt64)
        case later
        case received(amount: UInt64)
        case trust(payment: String, hub: String)
        /// A provider's or gateway's fee risen since its quote: `requote` to see the new price.
        case feeRose(agreed: UInt64, asked: UInt64, requote: () -> Void)
        /// A hub charging more than agreed, in the way of a payment: made `again` once approved.
        case approve(raised: FeesRaised, again: () -> Void)
        case failed(why: String, title: String = "Couldn't pay")

        /// No provider pays out as much as asked: about `most` is offered instead.
        case tooMuch(why: String, address: String, most: UInt64)
        /// A note the hub of a payment would not carry: paid without it if the user says.
        case noNote(code: String, amount: UInt64?, hub: String)

        /// Too little to pay with, but bitcoin held or on its way, not in the
        /// balance yet: added now if `addable`, or protected first if `offered`.
        case addFirst(why: String, addable: Bool, offered: Bool)

        /// Too little to pay with, and nothing held at all.
        case nothingYet(why: String)

        /// A coin on Bitcoin of `amount` being added to the balance.
        case adding(amount: UInt64)

        /// Which step this is, whatever it holds.
        var kind: String {
            switch self {
            case .looking: "looking"
            case .paying: "paying"
            case .planning: "planning"
            case .review: "review"
            case .visit: "visit"
            case .invoice: "invoice"
            case .address: "address"
            case .payout: "payout"
            case .busy: "busy"
            case .quoting: "quoting"
            case .payingOut: "payingOut"
            case .sent: "sent"
            case .paid: "paid"
            case .later: "later"
            case .received: "received"
            case .trust: "trust"
            case .feeRose: "feeRose"
            case .approve: "approve"
            case .failed: "failed"
            case .tooMuch: "tooMuch"
            case .noNote: "noNote"
            case .addFirst: "addFirst"
            case .nothingYet: "nothingYet"
            case .adding: "adding"
            }
        }
    }

    @Environment(\.blockSeconds) private var blockSeconds
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    // Opened with a link: no camera to start.
    @SwiftUI.State private var step: Step
    @SwiftUI.State private var empty = false
    // Protection offered, the balance being too little to pay.
    @SwiftUI.State private var protecting = false
    // This wallet's own address, once cashing out to it.
    @SwiftUI.State private var own: String?
    // A note for the payee of a code, and whether to tell them who paid.
    @SwiftUI.State private var note = ""
    @SwiftUI.State private var shareCode = false
    // Which: 0 the wallet's own, else an extra code's number.
    @SwiftUI.State private var shareAs: UInt32 = 0
    @SwiftUI.State private var codes: [ExtraCode] = []
    // Each step after a wait tells how long it took.
    @SwiftUI.State private var wait = Wait()
    // Calls under way, cancelled as Send goes: none pays or takes once it is gone.
    @SwiftUI.State private var running: [Task<Void, Never>] = []

    init(
        engine: Engine,
        ui: Ui,
        link: String?,
        cashOut: String? = nil,
        ownAddress: String? = nil,
        cashOutAmount: UInt64? = nil,
        toReceive: (() -> Void)? = nil,
        back: @escaping () -> Void
    ) {
        self.engine = engine
        self.ui = ui
        self.link = link
        self.cashOut = cashOut
        self.ownAddress = ownAddress
        self.cashOutAmount = cashOutAmount
        self.toReceive = toReceive
        self.back = back
        _step = SwiftUI.State(initialValue: link == nil ? .looking : .busy)
        _own = SwiftUI.State(initialValue: ownAddress)
    }

    // Too little to pay with: protection offered first, if a coin pays for
    // one, its rest joining the balance; else bitcoin held or coming, to
    // open, or nothing held, to receive; else why.
    private func tooLittle(e: Error, title: String = "Couldn't pay") -> Step {
        let why = explain(e: e)
        let b = ui.balance
        let bitcoin = b.onchain + b.moving + b.opening + ui.incoming.reduce(0) { $0 + $1.amount }
        let addable = addable(balance: b, opening: ui.opening != nil)
        let insufficient = if case LicashError.Insufficient = e { true } else { false }
        switch lack(insufficient: insufficient, noHub: noHub(e: e), notes: b.notes, bitcoin: bitcoin, offered: ui.offer != nil) {
        case .offer: return .addFirst(why: why, addable: addable, offered: true)
        case .bitcoin: return .addFirst(why: why, addable: addable, offered: false)
        case .nothing: return .nothingYet(why: why)
        case .other: return .failed(why: why, title: title)
        }
    }

    private func taking(payment: String, trust: Bool) async -> Step {
        switch await engine.receive(payment: payment, trust: trust) {
        case .success(let it):
            return .received(amount: it.amount)
        case .failure(let e):
            switch e {
            case let LicashError.UnknownHub(url): return .trust(payment: payment, hub: url)
            default: return .failed(why: explain(e: e), title: "Couldn't receive")
            }
        }
    }

    private func take(payment: String, trust: Bool) {
        step = .busy
        Task { step = await taking(payment: payment, trust: trust) }
    }

    private func quoting(address: String, amount: UInt64?) async -> Step {
        step = .quoting
        switch await engine.payoutQuote(address: address, amount: amount, hub: cashOut) {
        case .success(let it):
            return .payout(quote: it, asked: amount)
        case .failure(let e):
            // Too much for any provider: the most they pay out, if less, to send instead.
            let most: UInt64? = if noPayout(e: e) {
                offeredInstead(asked: amount, most: try? await engine.payoutMost(address: address, hub: cashOut).get())
            } else {
                nil
            }
            if let most { return .tooMuch(why: explain(e: e), address: address, most: most) }
            return tooLittle(e: e, title: "Can't send on Bitcoin")
        }
    }

    private func quote(address: String, amount: UInt64?) {
        Task { step = await quoting(address: address, amount: amount) }
    }

    private func read(text: String) {
        step = .busy
        running.append(Task {
            switch await engine.scanned(text: text) {
            case .success(let scanned):
                if Task.isCancelled { return }
                if cashOut != nil {
                    // Cashing out: to an address, nothing else, as much as chosen.
                    if case let .bitcoin(address, asked) = scanned {
                        if let asked {
                            step = await quoting(address: address, amount: asked)
                        } else {
                            step = .address(address: address)
                        }
                    } else {
                        step = .failed(why: "To cash out, scan or paste a Bitcoin address.", title: "Not a Bitcoin address")
                    }
                } else if case let .code(amount, notes) = scanned {
                    if notes {
                        step = .paying(code: text, asked: amount)
                    } else {
                        step = .failed(why: "This code names no hub you use, and no Bitcoin address to pay.", title: "Can't pay this")
                    }
                } else if case let .visit(amount) = scanned {
                    step = .visit(code: text, asked: amount)
                } else if case let .bitcoin(address, asked) = scanned {
                    if let asked {
                        step = await quoting(address: address, amount: asked)
                    } else {
                        step = .address(address: address)
                    }
                } else if case .payment = scanned {
                    step = await taking(payment: text, trust: false)
                } else if case .invoice = scanned {
                    switch await engine.invoiceQuote(text: text) {
                    case .success(let it): step = .invoice(text: text, quote: it)
                    case .failure(let e): step = .failed(why: explain(e: e))
                    }
                } else {
                    step = .failed(why: "Nothing to pay there.", title: "Nothing to pay")
                }
            case .failure(let e):
                step = .failed(why: explain(e: e), title: "Can't read this")
            }
        })
    }

    // Why a payment failed: its hub's fees to approve first, if it charges
    // more than agreed, and the payment made `again` then; a fee risen since
    // quoted, to `requote` and confirm at the new price.
    private func unpaid(e: Error, requote: (() -> Void)? = nil, again: @escaping () -> Void) async -> Step {
        if case let LicashError.FeeRose(_, agreed, asked, _) = e, let requote {
            return .feeRose(agreed: agreed, asked: asked, requote: requote)
        }
        var raised: FeesRaised?
        if case let LicashError.FeesRaised(hub) = e { raised = try? await engine.feesRaised(hub: hub).get() }
        guard let raised else { return .failed(why: explain(e: e)) }
        return .approve(raised: raised, again: again)
    }

    private func paying(code: String, amount: UInt64?) async -> Step {
        switch await engine.pay(code: code, amount: amount, note: noteOf(typed: note), share: shareCode ? shareAs : nil) {
        case .success(let it):
            return .sent(amount: it.amount, handover: it.handover, code: code, kept: it.kept)
        case .failure(let e):
            switch e {
            case LicashError.Pending:
                return .later
            case let LicashError.NoteUnkept(hub):
                return .noNote(code: code, amount: amount, hub: hub)
            default:
                return await unpaid(e: e, requote: { step = .paying(code: code, asked: amount) }) {
                    step = .busy
                    Task { step = await paying(code: code, amount: amount) }
                }
            }
        }
    }

    private func pay(code: String, amount: UInt64?) {
        step = .busy
        Task { step = await paying(code: code, amount: amount) }
    }

    // Quoted first: paid at once at a hub trusted holding enough for no
    // more than the fee `shown`, else reviewed, a swap, a hub not added or
    // a fee risen in the way.
    private func plan(code: String, amount: UInt64?, shown: UInt64?) {
        step = .planning
        running.append(Task {
            switch await engine.payQuote(code: code, amount: amount) {
            case .success(let quote):
                if Task.isCancelled { return }
                if quote.trusted && quote.swapFrom == nil, let shown, quote.fee <= shown {
                    step = .busy
                    step = await paying(code: code, amount: amount)
                } else {
                    step = .review(code: code, amount: amount, quote: quote)
                }
            case .failure(let e):
                step = tooLittle(e: e)
            }
        })
    }

    private func requoteInvoice(text: String) {
        step = .busy
        Task {
            switch await engine.invoiceQuote(text: text) {
            case .success(let it): step = .invoice(text: text, quote: it)
            case .failure(let e): step = .failed(why: explain(e: e))
            }
        }
    }

    private func payInvoice(text: String, quote: InvoiceQuote) {
        step = .busy
        Task {
            switch await engine.payInvoice(text: text, quote: quote) {
            case .success(let it):
                step = it.state == .done ? .paid(amount: it.amount, fee: it.fee) : .later
            case .failure(let e):
                step = await unpaid(e: e, requote: { requoteInvoice(text: text) }) { payInvoice(text: text, quote: quote) }
            }
        }
    }

    private func payOut(quote: PayoutQuote, asked: UInt64?) {
        step = .busy
        Task {
            switch await engine.payOut(quote: quote, asked: asked) {
            case .success(let it):
                step = .payingOut(amount: it.amount, depth: quote.depth)
            case .failure(let e):
                step = await unpaid(e: e, requote: { self.quote(address: quote.address, amount: asked) }) {
                    payOut(quote: quote, asked: asked)
                }
            }
        }
    }

    var body: some View {
        let offer = ui.opening == nil ? ui.offer : nil
        let waiting = switch step {
        case .busy, .planning, .quoting: true
        default: false
        }
        VStack(alignment: .leading, spacing: 0) {
            TopBar(title: cashOut == nil && ownAddress == nil ? "Send" : "Cash out", back: back)
            ScrollView {
                // Each step fades into the next; one that only changes what it shows does not.
                ZStack(alignment: .topLeading) {
                    // One column, as the step was before it faded: its parts stacked, not laid over each other.
                    VStack(alignment: .leading, spacing: 0) {
                        switch step {
                        case .looking:
                            if cashOut != nil {
                                Spacer().frame(height: 8)
                                BigButton(text: "To this wallet") {
                                    Task {
                                        switch await engine.bitcoinAddress() {
                                        case .success(let it):
                                            own = it
                                            step = .address(address: it)
                                        case .failure(let e):
                                            step = .failed(why: explain(e: e))
                                        }
                                    }
                                }
                                .frame(maxWidth: .infinity)
                                .accessibilityIdentifier("to-own")
                                Text("Or scan an address")
                                    .textStyle(Theme.type.labelLarge)
                                    .foregroundStyle(Theme.color.onSurfaceVariant)
                                    .multilineTextAlignment(.center)
                                    .frame(maxWidth: .infinity)
                                    .padding(.top, 20)
                                    .padding(.bottom, 8)
                            }
                            Scanner { read(text: $0) }
                                .frame(maxWidth: .infinity)
                                .aspectRatio(1, contentMode: .fit)
                                .clipShape(RoundedRectangle(cornerRadius: 24, style: .continuous))
                            Spacer().frame(height: 16)
                            BigButton(text: "Paste", quiet: true) {
                                // Whether text is copied, which iOS tells without asking to paste: if pasting is not allowed, nothing is said.
                                let copied = UIPasteboard.general.hasStrings
                                let text = copied ? UIPasteboard.general.string?.trimmingCharacters(in: .whitespacesAndNewlines) : nil
                                empty = !copied || text?.isEmpty == true
                                if let text, !text.isEmpty { read(text: text) }
                            }
                            .frame(maxWidth: .infinity)
                            .accessibilityIdentifier("paste")
                            if empty {
                                Spacer().frame(height: 8)
                                Text("Nothing copied yet")
                                    .textStyle(Theme.type.bodySmall)
                                    .foregroundStyle(Theme.color.onSurfaceVariant)
                                    .multilineTextAlignment(.center)
                                    .frame(maxWidth: .infinity)
                            }
                            // Or someone paid before, by name.
                            if cashOut == nil { Contacts(engine: engine) { read(text: $0.code) } }
                        case let .paying(code, asked, visit):
                            Paying(engine: engine, ui: ui, code: code, asked: asked, visit: visit, noting: {
                                NoteFields(
                                    note: note,
                                    onNote: { note = $0 },
                                    share: shareCode,
                                    onShare: { shareCode = $0 },
                                    codes: codes,
                                    shareAs: shareAs,
                                    onShareAs: { shareAs = $0 }
                                )
                            }) { amount, shown in plan(code: code, amount: amount, shown: shown) }
                        case .busy:
                            Middle(text: "One moment…")
                        case .planning:
                            Middle(text: "One moment…")
                        case let .review(code, amount, quote, provider, through):
                            Payee(engine: engine, code: code) { to in
                                Review(quote: quote, to: to, through: ui.developer ? through : nil, pay: { pay(code: code, amount: amount) }) {
                                    // A swap first: any provider that could, to choose.
                                    if ui.developer && quote.swapFrom != nil {
                                        Providers {
                                            await engine.payOffers(code: code, amount: amount).map { offers in
                                                let swaps = offers.filter { $0.provider != nil }
                                                let current = provider ?? swaps.first?.provider
                                                return swaps.map { o in
                                                    let provider = o.provider!
                                                    let name = o.providerName ?? host(url: provider)
                                                    return Offer(
                                                        name: name,
                                                        detail: "Fees \(feeOf(fee: o.quote.fee, of: o.quote.amount))" + (o.known ? ", used before" : ""),
                                                        current: provider == current
                                                    ) {
                                                        step = .planning
                                                        Task {
                                                            switch await engine.choosePayOffer(code: code, amount: amount, provider: provider) {
                                                            case .success(let it):
                                                                step = .review(code: code, amount: amount, quote: it, provider: provider, through: name)
                                                            case .failure(let e):
                                                                step = .failed(why: explain(e: e))
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        case let .visit(code, asked):
                            Ask(
                                title: "Pay at their hub?",
                                why: "Their hub is not one of yours.",
                                action: "Pay there",
                                act: {
                                    if asked != nil {
                                        plan(code: code, amount: nil, shown: nil)
                                    } else {
                                        step = .paying(code: code, asked: asked, visit: true)
                                    }
                                },
                                back: back
                            )
                        case .quoting:
                            Middle(text: "Getting a price…")
                        case let .invoice(text, quote):
                            Invoice(quote: quote, several: ui.hubs.count > 1, pay: { payInvoice(text: text, quote: quote) }) {
                                if ui.developer {
                                    Providers {
                                        await engine.invoiceOffers(text: text).map { offers in
                                            offers.map { o in
                                                let through = o.provider.map { $0.name ?? host(url: $0.url) } ?? "\(name(url: o.hub))'s gateway"
                                                return Offer(
                                                    name: through,
                                                    detail: "Fees \(feeOf(fee: o.fee, of: o.amount)), from \(name(url: o.hub))",
                                                    current: o == quote
                                                ) { step = .invoice(text: text, quote: o) }
                                            }
                                        }
                                    }
                                }
                            }
                        case let .address(address):
                            ToAddress(
                                address: address,
                                own: address == own,
                                prefill: cashOut != nil ? cashOutAmount : nil,
                                all: cashOut != nil ? "Cash out all" : "Send all"
                            ) { amount in quote(address: address, amount: amount) }
                        case let .payout(quote, asked):
                            Payout(quote: quote, own: quote.address == own, several: ui.hubs.count > 1, pay: { payOut(quote: quote, asked: asked) }) {
                                if ui.developer {
                                    Providers {
                                        await engine.payoutOffers(address: quote.address, amount: asked, hub: quote.hub).map { offers in
                                            offers.map { o in
                                                Offer(
                                                    name: o.providerName ?? host(url: o.provider),
                                                    detail: "Fees \(feeOf(fee: o.fee, of: o.amount)), pays \(sat(amount: o.amount))",
                                                    current: o.provider == quote.provider
                                                ) { step = .payout(quote: o, asked: asked) }
                                            }
                                        }
                                    }
                                }
                            }
                        case let .payingOut(amount, depth):
                            Told(text: "Sending \(sat(amount: amount))", sub: "Arrives in \(arrives(depth: depth, blockSecs: blockSeconds))", back: back)
                        case let .paid(amount, fee):
                            Done(text: "Paid \(sat(amount: amount))", back: back, sub: fee > 0 ? "Fee \(feeOf(fee: fee, of: amount))" : nil)
                        case let .sent(amount, handover, code, kept):
                            Sent(amount: amount, handover: handover, kept: kept, back: back)
                            if let code { SaveContact(engine: engine, code: code) }
                        // Not to pay again: it finishes by itself.
                        case .later:
                            Told(text: "Under way", sub: "It finishes by itself", back: back)
                        case let .received(amount):
                            Done(text: "Received \(sat(amount: amount))", back: back)
                        case let .noNote(code, amount, hub):
                            Ask(
                                title: "Send without the note?",
                                why: "\(name(url: hub)) can't carry a note with a payment. They would get the payment, but not your note" +
                                    (shareCode ? " or your code." : "."),
                                action: "Send without the note",
                                act: {
                                    note = ""
                                    shareCode = false
                                    pay(code: code, amount: amount)
                                },
                                back: back
                            )
                        case let .trust(payment, hub):
                            Ask(
                                title: "Take it?",
                                why: "It is at \(name(url: hub)), a hub you don't use yet. Taking it adds that hub.",
                                action: "Take it",
                                act: { take(payment: payment, trust: true) },
                                back: back
                            )
                        case let .approve(raised, again):
                            Ask(
                                title: "Approve higher fees?",
                                why: "\(name(url: raised.hub)) raised its fees since your protection there started.\n\n" +
                                    raises(raised: raised).map { "\($0.what): \($0.now), was \($0.agreed)" }.joined(separator: "\n"),
                                action: "Approve and pay",
                                act: {
                                    step = .busy
                                    running.append(Task {
                                        switch await engine.approveFees(raised: raised) {
                                        case .success:
                                            if Task.isCancelled { return }
                                            again()
                                        case .failure(let e): step = .failed(why: explain(e: e))
                                        }
                                    })
                                },
                                back: back
                            )
                        case let .feeRose(agreed, asked, requote):
                            Ask(title: "The fee went up", why: feeRose(agreed: agreed, asked: asked), action: "See new price", act: requote, back: back)
                        case let .failed(why, title):
                            Ask(title: title, why: why, action: "Try again", act: { step = .looking }, back: back, failed: true)
                        case let .addFirst(why, addable, offered):
                            Outcome(
                                tone: .ask,
                                title: ui.balance.notes > 0 ? "Not enough in your balance" : "Your bitcoin isn't in your balance yet",
                                detail: "\(why)\n\n" + addFirst(addable: addable, offered: offered && offer != nil),
                                detailTag: "ask"
                            ) {
                                // On its way only: nothing to do but wait.
                                // Protected first where offered, as on the page for what is on Bitcoin.
                                let protectable = offered && offer != nil
                                if protectable {
                                    BigButton(text: "Add to balance, protected") {
                                        protecting = true
                                    }
                                    .frame(maxWidth: .infinity)
                                    .accessibilityIdentifier("protect-it")
                                    Spacer().frame(height: 12)
                                }
                                if addable {
                                    BigButton(text: protectable ? "Add without protection" : "Add to balance", quiet: protectable) {
                                        step = .busy
                                        Task {
                                            switch await engine.moveLargest() {
                                            case .success(let it): step = .adding(amount: it.amount)
                                            case .failure(let e): step = .failed(why: explain(e: e), title: "Couldn't add it")
                                            }
                                        }
                                    }
                                    .frame(maxWidth: .infinity)
                                    .accessibilityIdentifier("add-to-balance")
                                    Spacer().frame(height: 12)
                                }
                                BigButton(text: "Cancel", quiet: true, onClick: back)
                                    .frame(maxWidth: .infinity)
                                    .accessibilityIdentifier("cancel")
                            }
                        case let .adding(amount):
                            Told(text: "Adding \(sat(amount: amount))", sub: ADDING, back: back)
                        case let .tooMuch(why, address, most):
                            Outcome(
                                tone: .ask,
                                title: "Can't send that much on Bitcoin",
                                detail: "\(why)\n\nThe providers pay out about \(sat(amount: most)) now.",
                                detailTag: "too-much"
                            ) {
                                BigButton(text: "Use \(sat(amount: most))") {
                                    quote(address: address, amount: most)
                                }
                                .frame(maxWidth: .infinity)
                                .accessibilityIdentifier("use-most")
                                Spacer().frame(height: 12)
                                BigButton(text: "Cancel", quiet: true, onClick: back)
                                    .frame(maxWidth: .infinity)
                                    .accessibilityIdentifier("cancel")
                            }
                        case let .nothingYet(why):
                            Ask(
                                title: "Nothing to pay with yet",
                                why: "\(why)\n\n\(RECEIVE_FIRST)",
                                action: "Receive",
                                act: toReceive ?? back,
                                back: back
                            )
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .id(step.kind)
                    .transition(
                        reduceMotion ? AnyTransition.identity : AnyTransition.asymmetric(
                            insertion: .opacity.animation(.easeInOut(duration: 0.15).delay(0.05)),
                            removal: .opacity.animation(.easeInOut(duration: 0.05))
                        )
                    )
                }
                .animation(reduceMotion ? nil : .easeInOut(duration: 0.15), value: step.kind)
            }
            .environment(\.wait, wait)
        }
        .padding(.horizontal, 24)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .rememberWait(wait: wait, waiting: waiting)
        .sheet(isPresented: Binding(
            get: { protecting && offer != nil },
            set: {
                if !$0, protecting, engine.ui.opening == nil, engine.ui.offer != nil {
                    protecting = false
                    back()
                }
            }
        )) {
            if let offer {
                OfferSheet(engine: engine, q: offer, ready: !behind(ui: ui)) {
                    // The sheet calls this as it closes too, also when the offer goes: back once, only while it stands.
                    if protecting, engine.ui.opening == nil, engine.ui.offer != nil {
                        protecting = false
                        back()
                    }
                }
            }
        }
        .task { codes = (try? await engine.codes().get()) ?? [] }
        .task(id: link) { if let link { read(text: link) } }
        .onDisappear { for task in running { task.cancel() } }
    }
}

/// Why bitcoin held is not paid with yet, and what can be done: added now
/// if `addable`, else only waited for; protected first if `offered`.
func addFirst(addable: Bool, offered: Bool) -> String {
    if !addable {
        "You pay from your balance, and your bitcoin is still on its way to it. Try again once it has arrived."
    } else if offered {
        "You pay from your balance. Add your bitcoin to it: protect it first, covered by a contract " +
            "at your hub, for a fee, or add your largest coin now without protection."
    } else {
        "You pay from your balance. Add your largest coin to it now, held at your hub without protection."
    }
}

/// A coin being added to the balance, before it can be paid with.
let ADDING = "It joins your balance once confirmed on Bitcoin, in about an hour. Pay then."

/// What to do when nothing is held at all.
let RECEIVE_FIRST = "You pay from your balance, and it is empty. Receive some first."

/// One fact of a payment to confirm: `tag` names its value, for the tests; a fee's `share` of the amount.
private final class Fact {
    let label: String
    let value: String
    let tag: String
    let share: String?
    let fee: FeeBreakdown?

    init(label: String, value: String, tag: String, share: String? = nil, fee: FeeBreakdown? = nil) {
        self.label = label
        self.value = value
        self.tag = tag
        self.share = share
        self.fee = fee
    }
}

/// What a payment costs and when it arrives, to confirm it by: with what it comes to, if it costs.
private func costs(
    amount: UInt64,
    fee: UInt64,
    arrives: String,
    breakdown: FeeBreakdown? = nil
) -> [Fact] {
    [
        Fact(
            label: "Fee",
            value: fee == 0 ? "None" : sat(amount: fee),
            tag: "fee",
            share: share(fee: fee, of: amount),
            fee: breakdown ?? FeeBreakdown(amount: amount, fee: fee)
        ),
        fee > 0 ? Fact(label: "Total", value: sat(amount: amount &+ fee), tag: "total") : nil,
        Fact(label: "Arrives", value: arrives, tag: "arrives"),
    ].compactMap { $0 }
}

/// How long a payment on Bitcoin takes to count, `depth` blocks deep, of `blockSecs` each.
func arrives(depth: UInt32, blockSecs: UInt32 = BITCOIN_BLOCK_SECONDS) -> String {
    "about \(minutes(blocks: Int64(depth) + 1, blockSecs: blockSecs)) min"
}

/// A payment to confirm: how much, then what there is to know before
/// agreeing, a fact a line, and one button. `head` goes under the amount,
/// `more` under the button.
private struct Confirm<Head: View, More: View>: View {
    let amount: UInt64
    let facts: [Fact]
    let action: String
    let act: () -> Void
    let head: Head
    let more: More

    init(
        amount: UInt64,
        facts: [Fact],
        action: String,
        act: @escaping () -> Void,
        @ViewBuilder head: () -> Head,
        @ViewBuilder more: () -> More
    ) {
        self.amount = amount
        self.facts = facts
        self.action = action
        self.act = act
        self.head = head()
        self.more = more()
    }

    var body: some View {
        VStack(alignment: .center, spacing: 0) {
            Spacer().frame(height: 32)
            Text(sat(amount: amount))
                .textStyle(Heading)
                .accessibilityIdentifier("paying")
            head
            Facts(facts: facts)
            Waited()
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.top, 8)
            Spacer().frame(height: 24)
            BigButton(text: action, onClick: act)
                .frame(maxWidth: .infinity)
                .accessibilityIdentifier("confirm")
            more
        }
        .frame(maxWidth: .infinity)
    }
}

extension Confirm where Head == EmptyView {
    init(amount: UInt64, facts: [Fact], action: String, act: @escaping () -> Void, @ViewBuilder more: () -> More) {
        self.init(amount: amount, facts: facts, action: action, act: act, head: { EmptyView() }, more: more)
    }
}

private struct Facts: View {
    let facts: [Fact]

    var body: some View {
        Group(title: "") {
            ForEach(Array(facts.enumerated()), id: \.offset) { i, fact in
                if i > 0 { Divider() }
                if let fee = fact.fee {
                    FeeLine(label: fact.label, fee: fee.fee, amount: fee.amount, tag: fact.tag, value: fact.value, breakdown: fee)
                } else {
                    Line(label: fact.label, value: fact.value, tag: fact.tag, share: fact.share)
                }
            }
        }
    }
}

/// A code to pay in notes: the amount it asks, or one typed straight in.
/// The keyboard's action only hides it: the button pays. At a hub not
/// added, what it costs is told next, once known.
private struct Paying<Noting: View>: View {
    let engine: Engine
    let ui: Ui
    let code: String
    let asked: UInt64?
    let visit: Bool
    let noting: Noting
    let pay: (UInt64?, UInt64?) -> Void

    @SwiftUI.State private var amount: Int64?
    // What the wallet pays as things stand, at a hub trusted holding
    // enough: none known, a swap or a visit in the way, told next.
    @SwiftUI.State private var exact: UInt64?

    init(
        engine: Engine,
        ui: Ui,
        code: String,
        asked: UInt64?,
        visit: Bool,
        @ViewBuilder noting: () -> Noting,
        pay: @escaping (UInt64?, UInt64?) -> Void
    ) {
        self.engine = engine
        self.ui = ui
        self.code = code
        self.asked = asked
        self.visit = visit
        self.noting = noting()
        self.pay = pay
        _amount = SwiftUI.State(initialValue: asked.map { Int64(truncatingIfNeeded: $0) })
    }

    var body: some View {
        // Else the most a hub would charge: the code may name several.
        let estimatedHub: Hub? = amount.flatMap { a in
            visit ? nil : ui.hubs.max {
                payingUpTo(hub: $0, amount: UInt64(truncatingIfNeeded: a)) < payingUpTo(hub: $1, amount: UInt64(truncatingIfNeeded: a))
            }
        }
        let fee: UInt64? = exact ?? amount.flatMap { a in estimatedHub.map { payingUpTo(hub: $0, amount: UInt64(truncatingIfNeeded: a)) } }
        let feeValue: String = fee.map { $0 == 0 ? "None" : exact != nil ? sat(amount: $0) : "Up to \(sat(amount: $0))" } ?? ""
        let feeShare: String? = fee.flatMap { f in amount.flatMap { share(fee: f, of: UInt64(truncatingIfNeeded: $0)) } }
        let breakdown: FeeBreakdown? = fee.flatMap { f in
            amount.flatMap { a in
                estimatedHub.map { hub in
                    FeeBreakdown(
                        amount: UInt64(truncatingIfNeeded: a),
                        fee: f,
                        components: [(exact != nil ? "Hub" : "Hub (estimate)", f)],
                        rate: LicashCore.Rate(base: hub.feeBase, ppm: hub.feePpm),
                        explanation: exact != nil
                            ? "What paying costs now, the notes it spends and makes counted."
                            : "Upper estimate using current terms at \(name(url: hub.url)), with its fee for each note made. The hub used to pay may charge less."
                    )
                }
            }
        }
        VStack(alignment: .center, spacing: 0) {
            Spacer().frame(height: 32)
            switch asked {
            case nil:
                AmountField(amount: amount, onAmount: { amount = $0 })
            case let asked?:
                Text(sat(amount: asked))
                    .textStyle(Heading)
                    .accessibilityIdentifier("paying")
            }
            if !visit {
                Payee(engine: engine, code: code) { to in
                    Facts(facts: [
                        Fact(label: "To", value: to, tag: "to"),
                        Fact(label: "Fee", value: feeValue, tag: "fee", share: feeShare, fee: breakdown),
                        Fact(label: "Arrives", value: "Instantly", tag: "arrives"),
                    ])
                }
            }
            Spacer().frame(height: 16)
            noting
            Spacer().frame(height: 24)
            BigButton(text: visit ? "Continue" : "Pay", enabled: amount != nil) {
                pay(asked == nil ? amount.map { UInt64(truncatingIfNeeded: $0) } : nil, exact)
            }
            .frame(maxWidth: .infinity)
            .accessibilityIdentifier("confirm")
        }
        .frame(maxWidth: .infinity)
        .task(id: [AnyHashable(amount), AnyHashable(ui.balance)]) {
            exact = nil
            guard let a = amount, !visit, a > 0 else { return }
            do { try await Task.sleep(for: .milliseconds(250)) } catch { return }
            let fee = try? await engine.payFee(code: code, amount: asked == nil ? UInt64(truncatingIfNeeded: a) : nil).get()
            if !Task.isCancelled { exact = fee }
        }
    }
}

/// A payment in notes to review: what it pays and costs, the swap bringing
/// notes to its hub first, and whether that hub is one not added.
private struct Review<More: View>: View {
    let quote: PayQuote
    let to: String
    let through: String?
    let pay: () -> Void
    let more: More

    init(quote: PayQuote, to: String, through: String?, pay: @escaping () -> Void, @ViewBuilder more: () -> More) {
        self.quote = quote
        self.to = to
        self.through = through
        self.pay = pay
        self.more = more()
    }

    var body: some View {
        let swap = quote.swapFrom
        let facts: [Fact] = [Fact(label: "To", value: to, tag: "to")]
            + costs(amount: quote.amount, fee: quote.fee, arrives: swap == nil ? "Instantly" : "Shortly")
            + [
                swap.map { Fact(label: "Moved first", value: "\(sat(amount: quote.swapped)) from \(name(url: $0))", tag: "moves") },
                through.map { Fact(label: "Through", value: $0, tag: "through") },
            ].compactMap { $0 }
        Confirm(
            amount: quote.amount,
            facts: facts,
            action: "Pay",
            act: pay,
            head: {
                if !quote.trusted {
                    Text("At \(name(url: quote.hub)), a hub you have not added")
                        .textStyle(Theme.type.bodyMedium)
                        .foregroundStyle(Theme.color.onSurfaceVariant)
                        .multilineTextAlignment(.center)
                        .accessibilityIdentifier("not-added")
                        .padding(.top, 8)
                }
            },
            more: { more }
        )
    }
}

/// A provider's offer, in a list to choose from: `choose` takes it.
private final class Offer {
    let name: String
    let detail: String
    let current: Bool
    let choose: () -> Void

    init(name: String, detail: String, current: Bool, choose: @escaping () -> Void) {
        self.name = name
        self.detail = detail
        self.current = current
        self.choose = choose
    }
}

/// Every provider's offer, once asked, `load`ing them: to choose another
/// than the one picked, in advanced mode.
private struct Providers: View {
    let load: () async -> Result<[Offer], Error>

    @SwiftUI.State private var offers: [Offer]?
    @SwiftUI.State private var asking = false
    @SwiftUI.State private var error: String?
    @SwiftUI.State private var wait = Wait()

    init(load: @escaping () async -> Result<[Offer], Error>) {
        self.load = load
    }

    var body: some View {
        // On the one part always shown, the body being several views.
        Spacer().frame(height: 16)
            .rememberWait(wait: wait, waiting: asking)
        switch offers {
        case nil:
            TextButton(
                onClick: {
                    asking = true
                    error = nil
                    Task {
                        switch await load() {
                        case .success(let it): offers = it
                        case .failure(let e): error = explain(e: e)
                        }
                        asking = false
                    }
                },
                enabled: !asking
            ) { Text(asking ? "Asking the providers…" : "All providers") }
                .accessibilityIdentifier("providers")
            Waited(wait: wait).padding(.leading, 12)
        case let list?:
            Text(list.count == 1 ? "The only provider" : "\(list.count) providers")
                .textStyle(Theme.type.titleSmall)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.top, 8)
            Waited(wait: wait)
            ForEach(Array(list.enumerated()), id: \.offset) { _, o in
                HStack(alignment: .center, spacing: 0) {
                    VStack(alignment: .leading, spacing: 0) {
                        Text(o.name)
                            .textStyle(Theme.type.bodyLarge)
                            .lineLimit(1)
                            .truncationMode(.tail)
                        Text(o.detail)
                            .textStyle(Theme.type.bodySmall)
                            .foregroundStyle(Theme.color.onSurfaceVariant)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    if o.current {
                        Text("Chosen")
                            .textStyle(Theme.type.labelLarge)
                            .foregroundStyle(Theme.color.primary)
                    } else {
                        TextButton(onClick: o.choose) { Text("Use") }
                            .accessibilityIdentifier("use \(o.name)")
                    }
                }
                .frame(maxWidth: .infinity)
                .padding(.vertical, 6)
            }
        }
        if let error {
            Text(error)
                .textStyle(Theme.type.bodySmall)
                .foregroundStyle(Theme.color.error)
        }
    }
}

/// A Lightning invoice to pay: what it asks, and all it costs on top; from
/// which hub, with `several`; through which swap provider, if one, and how
/// long its sats wait should it not pay. `more` under the button.
private struct Invoice<More: View>: View {
    let quote: InvoiceQuote
    let several: Bool
    let pay: () -> Void
    let more: More

    @Environment(\.blockSeconds) private var blockSeconds

    init(quote: InvoiceQuote, several: Bool, pay: @escaping () -> Void, @ViewBuilder more: () -> More) {
        self.quote = quote
        self.several = several
        self.pay = pay
        self.more = more()
    }

    var body: some View {
        let provider = quote.provider
        let facts: [Fact] = costs(amount: quote.amount, fee: quote.fee, arrives: "In seconds", breakdown: invoiceFee(quote: quote))
            + [
                several || provider != nil ? Fact(label: "From", value: name(url: quote.hub), tag: "from") : nil,
                provider.map { Fact(label: "Through", value: $0.name ?? host(url: $0.url), tag: "through") },
            ].compactMap { $0 }
        Confirm(
            amount: quote.amount,
            facts: facts,
            action: "Pay",
            act: pay,
            head: {
                if let description = quote.description {
                    Text(description)
                        .textStyle(Theme.type.bodyLarge)
                        .multilineTextAlignment(.center)
                        .lineLimit(3)
                        .truncationMode(.tail)
                        .padding(.top, 8)
                }
            },
            more: {
                if let provider {
                    Text(refundWait(blocks: provider.refundBlocks, blockSecs: blockSeconds))
                        .textStyle(Theme.type.bodySmall)
                        .foregroundStyle(Theme.color.onSurfaceVariant)
                        .multilineTextAlignment(.center)
                        .frame(maxWidth: .infinity)
                        .padding(.top, 12)
                        .accessibilityIdentifier("refund-wait")
                }
                more
            }
        )
    }
}

/// A Bitcoin address to pay: how much, typed straight in, or all. The
/// keyboard's action continues to what it costs, to confirm.
private struct ToAddress: View {
    let address: String
    let own: Bool
    let prefill: UInt64?
    let all: String
    let quote: (UInt64?) -> Void

    @SwiftUI.State private var amount: Int64?

    init(address: String, own: Bool, prefill: UInt64?, all: String, quote: @escaping (UInt64?) -> Void) {
        self.address = address
        self.own = own
        self.prefill = prefill
        self.all = all
        self.quote = quote
        _amount = SwiftUI.State(initialValue: prefill.map { Int64(truncatingIfNeeded: $0) })
    }

    var body: some View {
        VStack(alignment: .center, spacing: 0) {
            Spacer().frame(height: 32)
            AmountField(amount: amount, onAmount: { amount = $0 }, action: "Go") {
                if let amount { quote(UInt64(truncatingIfNeeded: amount)) }
            }
            Text(own ? "To this wallet" : "To \(shortAddress(address: address))")
                .textStyle(Theme.type.bodySmall)
                .foregroundStyle(Theme.color.onSurfaceVariant)
                .accessibilityIdentifier("to")
            Spacer().frame(height: 32)
            BigButton(text: "Continue", enabled: amount != nil) {
                if let amount { quote(UInt64(truncatingIfNeeded: amount)) }
            }
            .frame(maxWidth: .infinity)
            .accessibilityIdentifier("continue")
            Spacer().frame(height: 12)
            BigButton(text: all, quiet: true) { quote(nil) }
                .frame(maxWidth: .infinity)
                .accessibilityIdentifier("all")
        }
        .frame(maxWidth: .infinity)
    }
}

/// A payout on Bitcoin to confirm: what the address gets, all it costs on
/// top and when it arrives; from which hub, with `several`.
private struct Payout<More: View>: View {
    let quote: PayoutQuote
    let own: Bool
    let several: Bool
    let pay: () -> Void
    let more: More

    @Environment(\.blockSeconds) private var blockSeconds

    init(quote: PayoutQuote, own: Bool, several: Bool, pay: @escaping () -> Void, @ViewBuilder more: () -> More) {
        self.quote = quote
        self.own = own
        self.several = several
        self.pay = pay
        self.more = more()
    }

    var body: some View {
        let arriving = arrives(depth: quote.depth, blockSecs: blockSeconds)
        let facts: [Fact] = [Fact(label: "To", value: own ? "This wallet" : shortAddress(address: quote.address), tag: "to")]
            + costs(
                amount: quote.amount,
                fee: quote.fee,
                arrives: arriving.prefix(1).uppercased() + arriving.dropFirst(),
                breakdown: payoutFee(
                    amount: quote.amount,
                    fee: quote.fee,
                    hub: quote.lockFee,
                    provider: quote.providerFee,
                    network: quote.claimFee,
                    quoted: true
                )
            )
            + [several ? Fact(label: "From", value: name(url: quote.hub), tag: "from") : nil].compactMap { $0 }
        Confirm(amount: quote.amount, facts: facts, action: "Send", act: pay, more: { more })
    }
}

/// A long address, its ends shown.
private func shortAddress(address: String) -> String {
    address.count <= 20 ? address : address.prefix(10) + "…" + address.suffix(8)
}

/// The wallet the code `code` pays, to check before paying: its contact's
/// name if one, else the code shortened. A contact's is never shown of
/// another wallet's code.
private struct Payee<Content: View>: View {
    let engine: Engine
    let code: String
    let content: (String) -> Content

    @SwiftUI.State private var name: String?

    init(engine: Engine, code: String, @ViewBuilder content: @escaping (String) -> Content) {
        self.engine = engine
        self.code = code
        self.content = content
    }

    var body: some View {
        content(name ?? shortCode(code: code))
            .task(id: code) {
                name = nil
                let found = try? await engine.contactOf(code: code).get()
                if !Task.isCancelled { name = found?.name }
            }
    }
}

/// A payment code shortened: its text without scheme or amount, as a QR code's in upper case too.
func shortCode(code: String) -> String {
    var body = Substring(code.trimmingCharacters(in: .whitespacesAndNewlines))
    if let colon = body.firstIndex(of: ":") { body = body[body.index(after: colon)...] }
    if let query = body.firstIndex(of: "?") { body = body[..<query] }
    return shortAddress(address: body.lowercased())
}

/// Paid; if not to the payee's mailbox, what to hand them.
private struct Sent: View {
    let amount: UInt64
    let handover: String?
    let kept: Bool
    let back: () -> Void

    var body: some View {
        if let handover {
            // Its own layout, not an outcome's: felt the same, once.
            let feel = rememberFeel()
            VStack(alignment: .center, spacing: 0) {
                Spacer().frame(height: 32)
                Text("Sent \(sat(amount: amount))")
                    .textStyle(Theme.type.headlineSmall)
                Text(kept ? "They collect it from the hub. If they can't, hand them this" : "Hand them this to finish")
                    .textStyle(Theme.type.bodyMedium)
                    .foregroundStyle(Theme.color.onSurfaceVariant)
                    .multilineTextAlignment(.center)
                Waited(center: true).padding(.top, 8)
                Spacer().frame(height: 16)
                if handover.utf16.count <= 1500 { QrCode(text: handover) }
                Spacer().frame(height: 16)
                // A ShareLink drawn as a BigButton, which runs an action and cannot share.
                ShareLink(item: handover) {
                    Text("Share")
                        .textStyle(Theme.type.labelLarge)
                        .lineLimit(1)
                        .foregroundStyle(Theme.color.onPrimary)
                        .frame(maxWidth: .infinity)
                        .frame(height: 56)
                        .background(Theme.color.primary, in: RoundedRectangle(cornerRadius: 18, style: .continuous))
                }
                .buttonStyle(.plain)
                Spacer().frame(height: 12)
                BigButton(text: "Done", quiet: true, onClick: back)
                    .frame(maxWidth: .infinity)
                    .accessibilityIdentifier("done")
            }
            .frame(maxWidth: .infinity)
            .task { feel.confirm() }
        } else {
            Done(text: "Sent \(sat(amount: amount))", back: back)
        }
    }
}

/// What ``Outcome`` says in its glyph: done, under way, a question, or failed.
private enum Tone { case done, busy, ask, failed }

/// Where paying ends, or stops to ask, all alike: a glyph in a disc, a
/// title, a line under it, and its buttons. `titleTag` and `detailTag`
/// name them, for the tests.
private struct Outcome<Buttons: View>: View {
    let tone: Tone
    let title: String
    let detail: String?
    let titleTag: String?
    let detailTag: String?
    let buttons: Buttons

    init(
        tone: Tone,
        title: String,
        detail: String? = nil,
        titleTag: String? = nil,
        detailTag: String? = nil,
        @ViewBuilder buttons: () -> Buttons
    ) {
        self.tone = tone
        self.title = title
        self.detail = detail
        self.titleTag = titleTag
        self.detailTag = detailTag
        self.buttons = buttons()
    }

    var body: some View {
        let colors = Theme.color
        let tint = switch tone {
        case .failed: colors.error
        case .ask: colors.onSurfaceVariant
        default: colors.primary
        }
        // Felt once, as it shows: done, or failed. A wait or a question is not.
        let feel = rememberFeel()
        VStack(alignment: .center, spacing: 0) {
            Spacer().frame(height: 72)
            ZStack {
                switch tone {
                case .busy:
                    CircularProgressIndicator(size: 40, strokeWidth: 3).tint(tint)
                case .done:
                    Image(systemName: "checkmark").font(.system(size: 44)).foregroundStyle(tint).accessibilityHidden(true)
                case .ask:
                    Image(systemName: "info.circle").font(.system(size: 44)).foregroundStyle(tint).accessibilityHidden(true)
                case .failed:
                    Image(systemName: "exclamationmark.triangle.fill").font(.system(size: 40)).foregroundStyle(tint).accessibilityHidden(true)
                }
            }
            .frame(width: 88, height: 88)
            .background(tint.opacity(0.14), in: Circle())
            Spacer().frame(height: 24)
            Text(title)
                .textStyle(Theme.type.headlineSmall)
                .multilineTextAlignment(.center)
                .accessibilityIdentifier(titleTag ?? "")
                // Said by a screen reader as it shows.
                .task(id: title) { AccessibilityNotification.Announcement(title).post() }
            if let detail {
                Spacer().frame(height: 8)
                Text(detail)
                    .textStyle(Theme.type.bodyLarge)
                    .foregroundStyle(colors.onSurfaceVariant)
                    .multilineTextAlignment(.center)
                    .accessibilityIdentifier(detailTag ?? "")
            }
            Waited(center: true).padding(.top, 8)
            Spacer().frame(height: 40)
            buttons
        }
        .frame(maxWidth: .infinity)
        .task(id: tone) {
            switch tone {
            case .done: feel.confirm()
            case .failed: feel.reject()
            default: break
            }
        }
    }
}

extension Outcome where Buttons == EmptyView {
    init(tone: Tone, title: String, detail: String? = nil, titleTag: String? = nil, detailTag: String? = nil) {
        self.init(tone: tone, title: title, detail: detail, titleTag: titleTag, detailTag: detailTag) { EmptyView() }
    }
}

/// Waiting on something quick: what, under a spinner.
private struct Middle: View {
    let text: String

    var body: some View { Outcome(tone: .busy, title: text) }
}

/// It went through: said in a few words, `sub` under them if any.
private struct Done: View {
    let text: String
    let back: () -> Void
    var sub: String? = nil

    var body: some View {
        Outcome(tone: .done, title: text, detail: sub, titleTag: "result") {
            BigButton(text: "Done", onClick: back)
                .frame(maxWidth: .infinity)
                .accessibilityIdentifier("done")
        }
    }
}

/// It is under way: what, and what to expect of it.
private struct Told: View {
    let text: String
    let sub: String
    let back: () -> Void

    var body: some View {
        Outcome(tone: .busy, title: text, detail: sub, titleTag: "result") {
            BigButton(text: "Done", onClick: back)
                .frame(maxWidth: .infinity)
                .accessibilityIdentifier("done")
        }
    }
}

/// A question, or what went wrong `failed`: its `title`, why under it
/// (tagged `ask`), and what to do about it or leave.
private struct Ask: View {
    let title: String
    let why: String
    let action: String
    let act: () -> Void
    let back: () -> Void
    var failed = false

    var body: some View {
        Outcome(tone: failed ? .failed : .ask, title: title, detail: why, detailTag: "ask") {
            BigButton(text: action, onClick: act)
                .frame(maxWidth: .infinity)
                .accessibilityIdentifier("confirm")
            Spacer().frame(height: 12)
            BigButton(text: "Cancel", quiet: true, onClick: back)
                .frame(maxWidth: .infinity)
                .accessibilityIdentifier("cancel")
        }
    }
}
