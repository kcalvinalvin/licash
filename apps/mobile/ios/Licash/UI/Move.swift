import LicashCore
import SwiftUI

/// Move notes from `from` to another of the wallet's hubs, through a swap
/// provider, in a sheet: the hub to move to and the amount (all, if none),
/// then the providers' offers to choose from, the cheapest first. If the
/// chosen one fails before locking the notes, the next costing no more is
/// tried. It stays open while a move is under way.
struct MoveSheet: View {
    let engine: Engine
    let from: Holding
    let others: [Holding]
    let moved: () -> Void
    let dismiss: () -> Void

    @SwiftUI.State private var to: String?
    @SwiftUI.State private var text = ""
    @SwiftUI.State private var offers: [MoveQuote]?
    @SwiftUI.State private var chosen = 0
    @SwiftUI.State private var busy = false
    @SwiftUI.State private var error: String?
    @SwiftUI.State private var wait = Wait()

    init(engine: Engine, from: Holding, hubs: [Holding], moved: @escaping () -> Void, dismiss: @escaping () -> Void) {
        self.engine = engine
        self.from = from
        others = hubs.filter { $0.hub != from.hub }
        self.moved = moved
        self.dismiss = dismiss
        _to = SwiftUI.State(initialValue: others.first?.hub)
    }

    var body: some View {
        let shown = offers
        Sheet(title: "Move from \(name(url: from.hub))", dismiss: dismiss, dismissable: !busy) {
            switch shown {
            case nil:
                Group(title: "To") {
                    ForEach(Array(others.enumerated()), id: \.offset) { i, other in
                        if i > 0 { Divider() }
                        Choice(on: to == other.hub, title: name(url: other.hub), value: sat(amount: other.available)) { to = other.hub }
                    }
                }
                Spacer().frame(height: 16)
                OutlinedTextField(
                    value: text,
                    onValueChange: { typed in
                        // Decimal digits of any script, each one UTF-16 unit, kept as ASCII ones for UInt64 to read.
                        let digits = typed.unicodeScalars
                            .filter { $0.value <= 0xFFFF && $0.properties.generalCategory == .decimalNumber }
                            .compactMap { Character($0).wholeNumberValue }
                        text = String(digits.map(String.init).joined().prefix(15))
                    },
                    label: "Amount",
                    suffix: "sat",
                    supportingText: "All \(sat(amount: from.available)) if empty",
                    singleLine: true
                )
                .keyboardType(.numberPad)
                .frame(maxWidth: .infinity)
                .accessibilityIdentifier("move-amount")
            case let shown?:
                Group(title: "Offers") {
                    ForEach(Array(shown.enumerated()), id: \.offset) { i, o in
                        if i > 0 { Divider() }
                        Choice(
                            on: chosen == i,
                            title: "\(sat(amount: o.amount)) arrive",
                            value: "Fee \(feeOf(fee: o.fee, of: o.amount))",
                            sub: "Through \(o.providerName ?? host(url: o.provider))"
                        ) { chosen = i }
                    }
                }
                if shown.indices.contains(chosen) {
                    let quote = shown[chosen]
                    let split: [(String, UInt64)] = quote.lockFee <= quote.fee ? [
                        ("Hub lock fee", quote.lockFee),
                        ("Swap provider", quote.fee - quote.lockFee),
                    ] : []
                    FeeLine(label: "Fee", fee: quote.fee, amount: quote.amount, breakdown: FeeBreakdown(
                        amount: quote.amount, fee: quote.fee, components: split,
                        explanation: "The hub lock fee is separate from the provider's quoted cost. No separate miner fee or base/rate terms were supplied."
                    ))
                }
                Say(text: "If it never arrives, your notes come back once the lock expires.")
            }
            if busy {
                LinearProgressIndicator()
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 12)
            }
            Waited(wait: wait)
            if let error { Problem(text: error) }
            Spacer().frame(height: 8)
            let target = to
            BigButton(text: shown == nil ? "See offers" : "Move", enabled: !busy && target != nil) {
                busy = true
                error = nil
                Task {
                    if shown == nil, let target {
                        let amount = UInt64(text).flatMap { $0 > 0 ? $0 : nil }
                        switch await engine.moveOffers(from: from.hub, to: target, amount: amount) {
                        case .success(let it):
                            offers = it
                            chosen = 0
                        case .failure(let e):
                            error = explain(e: e)
                        }
                    } else if let shown {
                        // The chosen first, then the rest as preferred.
                        let order = [shown[chosen]] + shown.enumerated().filter { $0.offset != chosen }.map(\.element)
                        switch await engine.moveNotes(offers: order) {
                        case .success:
                            moved()
                        case .failure(let e):
                            error = explain(e: e)
                            // Its fee rose: offered again at the new price, to accept or not.
                            if case let LicashError.FeeRose(_, agreed, asked, _) = e {
                                let amount = UInt64(text).flatMap { $0 > 0 ? $0 : nil }
                                if case .success(let it) = await engine.moveOffers(from: from.hub, to: shown[chosen].to, amount: amount) {
                                    offers = it
                                    chosen = 0
                                    error = feeRose(agreed: agreed, asked: asked)
                                }
                            }
                        }
                    }
                    busy = false
                }
            }
            .frame(maxWidth: .infinity)
            .accessibilityIdentifier("move-go")
        }
        .rememberWait(wait: wait, waiting: busy)
    }
}

/// One of a few to pick, as a row: a dot, what it is and a line under it, and its value.
private struct Choice: View {
    let on: Bool
    let title: String
    let value: String
    var sub: String? = nil
    let pick: () -> Void

    var body: some View {
        Button(action: pick) {
            HStack(alignment: .center, spacing: 0) {
                RadioButton(selected: on, onClick: pick)
                Spacer().frame(width: 4)
                VStack(alignment: .leading, spacing: 0) {
                    Text(title).textStyle(Theme.type.bodyLarge)
                    if let sub {
                        Text(sub)
                            .textStyle(Theme.type.bodySmall)
                            .foregroundStyle(Theme.color.onSurfaceVariant)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                Text(value)
                    .textStyle(Amounts)
                    .foregroundStyle(Theme.color.onSurfaceVariant)
            }
            .frame(maxWidth: .infinity, minHeight: 56)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}
