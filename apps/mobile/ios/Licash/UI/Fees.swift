import LicashCore
import SwiftUI

/// Only supplied or recorded components, never inferred from current terms.
struct FeeBreakdown: Equatable {
    var amount: UInt64
    var fee: UInt64
    var components: [(String, UInt64)] = []
    var rate: Rate? = nil
    var explanation: String = "This total includes the quoted costs. Separate hub, provider and miner charges were not supplied."
    var title: String = "Fee breakdown"
    var totalLabel: String = "Total fee"
    var amountLabel: String = "Amount"

    // Tuples are not Equatable: the components are compared one by one.
    static func == (a: FeeBreakdown, b: FeeBreakdown) -> Bool {
        a.amount == b.amount && a.fee == b.fee && a.components.elementsEqual(b.components, by: ==) &&
            a.rate == b.rate && a.explanation == b.explanation && a.title == b.title &&
            a.totalLabel == b.totalLabel && a.amountLabel == b.amountLabel
    }
}

func recordedFee(amount: UInt64, fee: UInt64, components: [(String, UInt64)] = []) -> FeeBreakdown {
    FeeBreakdown(
        amount: amount, fee: fee, components: components,
        explanation: "Recorded cost only. Base/rate terms and any further recipient split were not saved. A pending or refunded cost may not be final."
    )
}

func setupCost(amount: UInt64, cost: UInt64) -> FeeBreakdown {
    FeeBreakdown(
        amount: amount, fee: cost,
        explanation: "Setup cost relative to the maximum payout, not a payment fee. A separate hub-fee and reserve split is not supplied here; reserves may return during recovery.",
        title: "Setup cost", totalLabel: "Total cost", amountLabel: "Maximum payout"
    )
}

extension ContractFee {
    /// What protection costs, `fee`: its opening fee and interest, the reserve apart, as it is not spent.
    func cost() -> UInt64 { opening &+ interest &+ interestFunded }
}

/// What protection of `cap` sat chosen within `range` costs, itemized as its
/// hub prices it: its opening fee, and its interest by the year on the part
/// it funds and on the `funded` part hers. Not the reserve, which is hers
/// and comes back (``RESERVE_MEANS``), nor her part, which joins her balance.
func contractCost(range: ContractRange, cap: UInt64, fee: ContractFee, funded: UInt64) -> FeeBreakdown {
    let hubs = cap - min(funded, cap)
    let parts = [
        ("Hub's opening fee", fee.opening),
        ("Interest on the hub's part", fee.interest),
        ("Interest on your part", fee.interestFunded),
    ]
    let yours = funded == 0 ? "" :
        " and \(percent(ppm: UInt64(range.ppmYearFunded))) a year on the \(digits(amount: funded)) sat you fund yourself, " +
        "paid from your coin and joining your balance once its funding confirms"
    return FeeBreakdown(
        amount: cap, fee: fee.cost(), components: feeParts(fee: fee.cost(), parts: parts),
        explanation: "Interest: \(percent(ppm: UInt64(range.ppmYear))) a year on the \(digits(amount: hubs)) sat the hub " +
            "funds\(yours). Not in this cost: your reserve, which comes back to you.",
        title: "Cost", totalLabel: "Total cost", amountLabel: "Covers up to"
    )
}

/// What the hub's interest is for, said behind the (i) of the cost.
let INTEREST_MEANS = "To protect your balance at a hub, the hub locks its own bitcoin on " +
    "Bitcoin beside yours, for as long as the protection lasts: if the hub ever stops, the app claims " +
    "what you hold there from it. Interest pays the hub for locking it. Part of the cover can be your own " +
    "coin instead, which costs less and joins your balance once confirmed."

/// What the reserve is, said behind its (i).
let RESERVE_MEANS = "The reserve is yours, not a fee. It stays in the contract and pays the " +
    "Bitcoin fees if you ever recover your money without the hub. Otherwise it comes back to you when " +
    "the protection ends."

/// A fee's parts by who gets them, as quoted or recorded: those of 0 left
/// out, and none at all unless they add up to the whole fee.
func feeParts(fee: UInt64, parts: [(String, UInt64)]) -> [(String, UInt64)] {
    let shown = parts.filter { $0.1 > 0 }
    let sum = shown.reduce(UInt64(0)) { total, part in UInt64.max - total < part.1 ? UInt64.max : total + part.1 }
    return !shown.isEmpty && sum == fee ? shown : []
}

/// A payout's fee: the hub's on the lock, the payout provider's (its coin's
/// miners in it) and the claim's miners. `quoted`, before it is made.
func payoutFee(amount: UInt64, fee: UInt64, hub: UInt64, provider: UInt64, network: UInt64, quoted: Bool) -> FeeBreakdown {
    let parts = feeParts(fee: fee, parts: [("Hub", hub), ("Payout provider", provider), ("Bitcoin network", network)])
    let provided = "The payout provider's fee includes the miners of its own coin."
    return if parts.isEmpty {
        quoted ? FeeBreakdown(amount: amount, fee: fee) : recordedFee(amount: amount, fee: fee)
    } else if quoted {
        FeeBreakdown(amount: amount, fee: fee, components: parts, explanation: provided)
    } else {
        FeeBreakdown(amount: amount, fee: fee, components: parts, explanation: "As recorded. \(provided) Base and rate terms were not saved.")
    }
}

/// A Lightning invoice's fee as quoted: all the hub's gateway asks, or the
/// hub's on the lock and the swap provider's, Lightning routing in it.
func invoiceFee(quote: InvoiceQuote) -> FeeBreakdown {
    guard let provider = quote.provider else { return FeeBreakdown(amount: quote.amount, fee: quote.fee) }
    let parts = feeParts(fee: quote.fee, parts: [("Hub", provider.lockFee), ("Swap provider", provider.fee)])
    return parts.isEmpty ? FeeBreakdown(amount: quote.amount, fee: quote.fee)
        : FeeBreakdown(amount: quote.amount, fee: quote.fee, components: parts, explanation: "The swap provider's fee includes Lightning routing.")
}

/// A move's fee as recorded: the hub's on the lock and the swap provider's.
func swapFee(amount: UInt64, fee: UInt64, hub: UInt64, provider: UInt64) -> FeeBreakdown {
    let parts = feeParts(fee: fee, parts: [("Hub", hub), ("Swap provider", provider)])
    return parts.isEmpty ? recordedFee(amount: amount, fee: fee)
        : FeeBreakdown(amount: amount, fee: fee, components: parts, explanation: "As recorded. Base and rate terms were not saved.")
}

/// Match the SDK's base plus relative fee, rounded up and saturating.
func relativeFee(amount: UInt64, ppm: UInt64) -> UInt64 {
    // In 128 bits: the product, and the 999,999 that rounds it up.
    let numerator = amount.multipliedFullWidth(by: ppm)
    let (low, carry) = numerator.low.addingReportingOverflow(999_999)
    let high = numerator.high + (carry ? 1 : 0)
    // A quotient of more than 64 bits saturates.
    if high >= 1_000_000 { return UInt64.max }
    return UInt64(1_000_000).dividingFullWidth((high, low)).quotient
}

func feeDue(amount: UInt64, rate: Rate) -> UInt64 {
    let relative = relativeFee(amount: amount, ppm: rate.ppm)
    return UInt64.max - relative < rate.base ? UInt64.max : relative + rate.base
}

struct FeeLine: View {
    let label: String
    let fee: UInt64
    let amount: UInt64
    let tag: String
    let value: String
    let breakdown: FeeBreakdown
    @SwiftUI.State private var showing = false

    init(
        label: String,
        fee: UInt64,
        amount: UInt64,
        tag: String = "fee",
        value: String? = nil,
        breakdown: FeeBreakdown? = nil
    ) {
        self.label = label
        self.fee = fee
        self.amount = amount
        self.tag = tag
        self.value = value ?? (fee == 0 ? "None" : sat(amount: fee))
        self.breakdown = breakdown ?? FeeBreakdown(amount: amount, fee: fee)
    }

    var body: some View {
        Line(label: label, value: value, tag: tag, share: share(fee: fee, of: amount), onClick: { showing = true })
            .sheet(isPresented: $showing) { FeeSheet(fee: breakdown, dismiss: { showing = false }) }
    }
}

struct FeeSheet: View {
    let fee: FeeBreakdown
    let dismiss: () -> Void

    var body: some View {
        Sheet(title: fee.title, dismiss: dismiss) {
            Line(label: fee.amountLabel, value: sat(amount: fee.amount))
            ForEach(fee.components.indices, id: \.self) { i in
                let (recipient, amount) = fee.components[i]
                Line(label: recipient, value: sat(amount: amount), share: share(fee: amount, of: fee.amount))
            }
            if let rate = fee.rate {
                Line(label: "Base fee", value: sat(amount: rate.base))
                Line(label: "Rate", value: percent(ppm: rate.ppm))
                let relative = relativeFee(amount: fee.amount, ppm: rate.ppm)
                Line(label: "\(percent(ppm: rate.ppm)) × \(sat(amount: fee.amount))", value: sat(amount: relative))
                Say(text: "The relative fee is rounded up to a whole satoshi.")
            }
            Line(label: fee.totalLabel, value: sat(amount: fee.fee), share: share(fee: fee.fee, of: fee.amount))
            Say(text: fee.explanation)
        }
    }
}
