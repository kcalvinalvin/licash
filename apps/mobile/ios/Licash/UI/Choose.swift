import LicashCore
import SwiftUI

/// The protection offered for what was paid on Bitcoin ([Ui.offer]),
/// as [ChooseSheet]: a coin on Bitcoin added to the balance, protected,
/// once accepted, or declined so it is not offered unasked again. None if
/// no coin is taken whole by a contract within its hub's range.
struct OfferSheet: View {
    let engine: Engine
    let q: ContractQuote
    let ready: Bool
    let dismiss: () -> Void
    @SwiftUI.State private var holding: Holding? = nil

    init(engine: Engine, q: ContractQuote, ready: Bool, dismiss: @escaping () -> Void) {
        self.engine = engine
        self.q = q
        self.ready = ready
        self.dismiss = dismiss
    }

    var body: some View {
        let fitting = q.range.map { range in
            q.coins.filter { fitOf(q: q, range: range, blocks: q.suggestedBlocks, coin: $0) != nil }
        } ?? []
        if let range = q.range, !fitting.isEmpty {
            let ui = engine.ui
            ChooseSheet(
                q: q,
                range: range,
                coins: fitting,
                ready: ready,
                cover: hubCover(hub: q.hub, holding: holding, contracts: ui.contracts, notes: []),
                open: { chosen, price in
                    dismiss()
                    engine.openContract(hub: q.hub, maxPrice: price, chosen: chosen)
                },
                dismiss: dismiss,
                hub: engine.called(url: q.hub),
                title: "Add to balance, protected",
                verb: "Add",
                decline: {
                    engine.declineOffer(q: q)
                    dismiss()
                }
            )
            .onChange(of: q.hub) { holding = nil }
            .task(id: [AnyHashable(q.hub), AnyHashable(ui.balance)]) {
                let found = await engine.holdings()
                if Task.isCancelled { return }
                if case .success(let all) = found {
                    holding = all.first { sameHub(a: $0.hub, b: q.hub) }
                }
            }
        } else {
            // Nothing to offer: its caller presents no sheet then.
            EmptyView()
        }
    }
}

/// The contracts within `range` over `blocks` that `coin` pays for whole
/// ([contractFit]), one of each cap, smallest first: none if no contract
/// takes the coin. The coin is spent whole, the price and the rest of it her
/// part, issued back to her as notes. The rest of the cap is the hub's.
func fitsOf(q: ContractQuote, range: ContractRange, blocks: UInt32, coin: DepositCoin) -> [ContractFit] {
    fitsFrom(least: range.minCap) { least in
        contractFit(range: range, blocks: blocks, coin: coin.value, leastCap: least, dust: q.dust, denominations: q.denominations, notes: q.fundedNotes)
    }
}

/// Each contract `fit` gives of `least` sat at least and above, the smallest it finds above the last.
func fitsFrom(least: UInt64, fit: @escaping (_ least: UInt64) -> ContractFit?) -> [ContractFit] {
    var fits: [ContractFit] = []
    var from = least
    while true {
        guard let found = fit(from), found.cap >= from else { break }
        fits.append(found)
        if found.cap == UInt64.max { break }
        from = found.cap + 1
    }
    return fits
}

/// The contract offered at first of `fits` for a coin of `coin` sat, with
/// `missing` sat at its hub not protected yet: the smallest covering half
/// again both, room to be paid more, else the largest the hub allows for it.
func suggestedFit(fits: [ContractFit], coin: UInt64, missing: UInt64 = 0) -> ContractFit? {
    let both = coin + missing
    return fits.first { $0.cap >= both + both / 2 } ?? fits.last
}

/// The contract offered at first for `coin`, if one takes it whole.
func fitOf(q: ContractQuote, range: ContractRange, blocks: UInt32, coin: DepositCoin) -> ContractFit? {
    suggestedFit(fits: fitsOf(q: q, range: range, blocks: blocks, coin: coin), coin: coin.value)
}

/// Each cover some coin of `fits` (each coin's contracts, as [fitsOf])
/// opens whole, smallest first, with the coins paying it, smallest first:
/// the cover is chosen, then a coin that pays it.
func coversOf(fits: [(DepositCoin, [ContractFit])]) -> [(UInt64, [DepositCoin])] {
    let pairs: [(UInt64, DepositCoin)] = fits.flatMap { coin, its in its.map { ($0.cap, coin) } }
    return Dictionary(grouping: pairs, by: { $0.0 })
        .sorted { $0.key < $1.key }
        .map { cap, coins in (cap, coins.map { $0.1 }.sorted { $0.value < $1.value }) }
}

/// The cover offered at first: the one suggested for the smallest coin a
/// contract takes whole, with `missing` sat at its hub not protected yet.
func suggestedCover(fits: [(DepositCoin, [ContractFit])], missing: UInt64 = 0) -> UInt64? {
    fits.filter { !$0.1.isEmpty }.min { $0.0.value < $1.0.value }
        .flatMap { suggestedFit(fits: $0.1, coin: $0.0.value, missing: missing)?.cap }
}

/// What to cash out to herself to protect what she holds at a hub: a coin of `coin` sat opening `cap`.
struct CashOutFirst: Equatable {
    var cap: UInt64
    var coin: UInt64
}

/// With no coin on Bitcoin that pays for it, the coin to cash out to herself
/// first to protect the `missing` sat at a hub not protected yet: one paying
/// the price of the smallest of `sizes` covering half again them (else the
/// largest) and [FIXED_PART], her part, the hub funding the rest. If a
/// contract of that cap does not take it whole (`fit` of a coin at a least
/// cap, as [contractFit]), as an older hub bounding the cap by the coin
/// refuses, the next smaller that does. None with nothing missing.
func cashOutFirst(
    sizes: [UInt64],
    missing: UInt64,
    price: @escaping (_ cap: UInt64, _ funded: UInt64) -> UInt64,
    fit: @escaping (_ coin: UInt64, _ least: UInt64) -> ContractFit?
) -> CashOutFirst? {
    if missing == 0 { return nil }
    guard let wanted = sizes.first(where: { $0 >= missing + missing / 2 }) ?? sizes.last else { return nil }
    for cap in sizes.filter({ $0 <= wanted }).reversed() {
        let coin = price(cap, FIXED_PART) + FIXED_PART
        if let found = fit(coin, cap), found.cap == cap { return CashOutFirst(cap: cap, coin: coin) }
    }
    return nil
}

/// Protection of terms chosen within what its hub allows. First what is
/// protected at the hub once it opens (`cover`, the hub's now, with it), then
/// what it covers, chosen first, which decides the coins on Bitcoin that pay
/// it whole: the smallest at first. Then what it costs, the reserve, hers and
/// refundable, and what of the coin joins her balance once its funding
/// confirms: the coin is spent whole. How long, and the cost itemized, under
/// More. Offered unasked, it can be declined. Not paid for until the app is
/// caught up with Bitcoin (`ready`), as opening it needs.
///
/// With no coin to pay it, she is told to cash out to herself first.
struct ChooseSheet: View {
    let q: ContractQuote
    let range: ContractRange
    let coins: [DepositCoin]
    let ready: Bool
    let cover: HubCover
    let open: (Chosen, _ price: UInt64) -> Void
    let dismiss: () -> Void
    // The hub, as named to her ([called]).
    let hub: String
    let title: String
    let verb: String
    let decline: (() -> Void)?
    // With no coin paying it: cashing out a coin of so many sat to herself first, to pay it.
    let cashOut: ((UInt64) -> Void)?
    @Environment(\.blockSeconds) private var blockSecs
    @SwiftUI.State private var blocks: UInt32
    // The cover chosen, kept while some coin opens it. At first, or once
    // none does, the one suggested.
    @SwiftUI.State private var chosenCap: UInt64? = nil
    @SwiftUI.State private var chosenCoin: DepositCoin? = nil

    /// Titled "Protect at" the hub unless `title` says otherwise.
    init(
        q: ContractQuote,
        range: ContractRange,
        coins: [DepositCoin],
        ready: Bool,
        cover: HubCover,
        open: @escaping (Chosen, _ price: UInt64) -> Void,
        dismiss: @escaping () -> Void,
        hub: String,
        title: String? = nil,
        verb: String = "Protect",
        decline: (() -> Void)? = nil,
        cashOut: ((UInt64) -> Void)? = nil
    ) {
        self.q = q
        self.range = range
        self.coins = coins
        self.ready = ready
        self.cover = cover
        self.open = open
        self.dismiss = dismiss
        self.hub = hub
        self.title = title ?? "Protect at \(hub)"
        self.verb = verb
        self.decline = decline
        self.cashOut = cashOut
        _blocks = SwiftUI.State(initialValue: min(max(q.suggestedBlocks, range.minBlocks), range.maxBlocks))
    }

    var body: some View {
        let colors = Theme.color
        let download = shortFileSize(bytes: q.download)
        let fits = coins.map { ($0, fitsOf(q: q, range: range, blocks: blocks, coin: $0)) }
        let covers = coversOf(fits: fits)
        let cap = chosenCap.flatMap { c in covers.contains { $0.0 == c } ? c : nil } ?? suggestedCover(fits: fits, missing: cover.missing)
        let payers = covers.first { $0.0 == cap }?.1 ?? []
        let coin = chosenCoin.flatMap { payers.contains($0) ? $0 : nil } ?? payers.first
        let fit = coin.flatMap { c in fits.first { $0.0 == c }?.1.first { $0.cap == cap } }
        let capped = fit?.cap ?? range.minCap
        let funded = fit?.funded ?? 0
        // What the hub is paid: its fee and interest, then the reserve, hers.
        let price = contractPrice(range: range, cap: capped, blocks: blocks, funded: funded)
        let fee = contractFee(range: range, cap: capped, blocks: blocks, funded: funded)
        let cost = fee.cost()
        // It stops protecting a little before it ends.
        let protectsFor = blocks > q.afterAssert ? blocks - q.afterAssert : 0
        // None paying it: what she holds here protected by a coin cashed out to herself first.
        let first: CashOutFirst? = if fit == nil && cashOut != nil {
            cashOutFirst(
                sizes: contractSizes(range: range),
                missing: cover.missing,
                price: { c, f in contractPrice(range: range, cap: c, blocks: blocks, funded: f) },
                fit: { v, least in
                    contractFit(range: range, blocks: blocks, coin: v, leastCap: least, dust: q.dust, denominations: q.denominations, notes: q.fundedNotes)
                }
            )
        } else {
            nil
        }
        Sheet(title: title, dismiss: dismiss) {
            if fit != nil || first != nil {
                // Cashed out first, what she holds here less the coin, her part of it back.
                let after = if let first {
                    coverAfter(
                        now: {
                            var c = cover
                            c.held = cover.held - min(cover.held, first.coin)
                            return c
                        }(),
                        cap: first.cap,
                        funded: FIXED_PART
                    )
                } else {
                    coverAfter(now: cover, cap: capped, funded: funded)
                }
                CoverBar(cover: after)
                    .padding(.top, 4)
                    .padding(.bottom, 6)
                if let says = after.says {
                    Text("At \(hub) then: \(says.prefix(1).lowercased() + says.dropFirst())")
                        .textStyle(Theme.type.bodySmall)
                        .foregroundStyle(colors.onSurfaceVariant)
                        .padding(.bottom, 8)
                        .accessibilityIdentifier("contract-after")
                }
            }
            Line(label: "Covers up to", value: sat(amount: first?.cap ?? capped), tag: "quote-cap")
            // Larger, the hub funds more of it: room to be paid more, at its interest.
            if fit != nil && covers.count > 1 {
                let at = covers.firstIndex { $0.0 == capped } ?? -1
                SwiftUI.Slider(
                    value: Binding(
                        get: { Double(at) },
                        set: { chosenCap = covers[min(max(Int($0.rounded()), 0), covers.count - 1)].0 }
                    ),
                    in: 0...Double(covers.count - 1),
                    step: 1
                )
                .tint(colors.primary)
                .accessibilityIdentifier("contract-cap")
            }
            if fit != nil, let coin {
                if payers.count > 1 {
                    Text("Paid with")
                        .textStyle(Theme.type.bodyLarge)
                        .padding(.top, 8)
                    ScrollView(.horizontal) {
                        HStack(alignment: .top, spacing: 0) {
                            ForEach(payers, id: \.outpoint) { c in
                                FilterChip(selected: c == coin, onClick: { chosenCoin = c }, label: sat(amount: c.value))
                                    .padding(.trailing, 8)
                                    .accessibilityIdentifier("contract-coin")
                            }
                        }
                    }
                    .scrollIndicators(.hidden)
                } else {
                    Line(label: "Paid with", value: "\(sat(amount: coin.value)) on Bitcoin", tag: "contract-paid-with")
                }
                HStack(alignment: .center, spacing: 0) {
                    FeeLine(
                        label: "Cost", fee: cost, amount: capped, tag: "quote-price", value: sat(amount: cost),
                        breakdown: contractCost(range: range, cap: capped, fee: fee, funded: funded)
                    )
                    .frame(maxWidth: .infinity)
                    Info(title: "Interest", text: INTEREST_MEANS)
                }
                HStack(alignment: .center, spacing: 0) {
                    Line(label: "Reserve, refundable", value: sat(amount: range.reserve), tag: "contract-reserve")
                        .frame(maxWidth: .infinity)
                    Info(title: "Reserve", text: RESERVE_MEANS)
                }
                Line(label: "Into your balance", value: sat(amount: funded), tag: "contract-funded")
            }
            Line(label: "For", value: span(blocks: Int64(protectsFor), blockSecs: blockSecs), tag: "quote-for")
            if let first, let cashOut {
                let firstFee = contractFee(range: range, cap: first.cap, blocks: blocks, funded: FIXED_PART)
                FeeLine(
                    label: "Cost", fee: firstFee.cost(), amount: first.cap, tag: "quote-price", value: sat(amount: firstFee.cost()),
                    breakdown: contractCost(range: range, cap: first.cap, fee: firstFee, funded: FIXED_PART)
                )
                Line(label: "Reserve, refundable", value: sat(amount: range.reserve), tag: "contract-reserve")
                Say(text: CASH_OUT_FIRST)
                BigButton(text: "Cash out \(sat(amount: first.coin)) to yourself") {
                    cashOut(first.coin)
                }
                .frame(maxWidth: .infinity)
                .accessibilityIdentifier("cash-out-first")
            } else if coin == nil && coins.isEmpty {
                Say(text: "Paid from Bitcoin you hold, and you hold none yet. Receive some on Bitcoin, or cash out to yourself, then protect.")
            } else if fit == nil {
                Text(NO_FIT)
                    .textStyle(Theme.type.bodyMedium)
                    .foregroundStyle(colors.error)
                    .padding(.vertical, 8)
                    .accessibilityIdentifier("contract-problem")
            }
            More {
                if range.maxBlocks > range.minBlocks {
                    Text("How long")
                        .textStyle(Theme.type.bodyLarge)
                        .padding(.top, 8)
                    SwiftUI.Slider(
                        value: Binding(
                            get: { Double(blocks) },
                            set: { blocks = blocksAt(value: $0, least: range.minBlocks, most: range.maxBlocks) }
                        ),
                        in: Double(range.minBlocks)...Double(range.maxBlocks)
                    )
                    .tint(colors.primary)
                    .accessibilityIdentifier("contract-blocks")
                }
                if fit != nil {
                    Line(label: "The hub's part", value: sat(amount: capped - min(funded, capped)), tag: "contract-hub-part")
                    Line(label: "Opening fee", value: sat(amount: fee.opening), tag: "contract-opening-fee")
                    Line(
                        label: "Interest",
                        value: "\(sat(amount: fee.interest)) · \(percent(ppm: UInt64(range.ppmYear))) a year of \(digits(amount: capped - min(funded, capped)))",
                        tag: "contract-interest"
                    )
                    if funded > 0 && (range.ppmYearFunded > 0 || fee.interestFunded > 0) {
                        Line(
                            label: "Interest on yours",
                            value: "\(sat(amount: fee.interestFunded)) · \(percent(ppm: UInt64(range.ppmYearFunded))) a year",
                            tag: "contract-interest-funded"
                        )
                    }
                }
                Line(label: "Takes", value: "\(SETUP_TAKES) · \(download)")
            }
            .accessibilityElement(children: .contain)
            .accessibilityIdentifier("contract-more")
            let now = Int64(Date().timeIntervalSince1970)
            Text(duty(ends: day(seconds: now + Int64(protectsFor) * Int64(blockSecs), now: now)))
                .textStyle(Theme.type.bodySmall)
                .foregroundStyle(colors.onSurfaceVariant)
                .padding(.vertical, 8)
                .accessibilityIdentifier("contract-duty")
            Worth(mock: q.mock)
            Spacer().frame(height: 8)
            if let coin {
                BigButton(text: ready ? "\(verb) for \(sat(amount: cost))" : "Waiting for Bitcoin", enabled: fit != nil && ready) {
                    open(Chosen(cap: capped, blocks: blocks, coin: coin.outpoint), price)
                }
                .frame(maxWidth: .infinity)
                .accessibilityIdentifier("protect")
            }
            if let decline {
                Spacer().frame(height: 8)
                BigButton(text: "Not now", quiet: true, onClick: decline)
                    .frame(maxWidth: .infinity)
                    .accessibilityIdentifier("decline-offer")
            }
        }
        .onChange(of: q) {
            blocks = min(max(q.suggestedBlocks, range.minBlocks), range.maxBlocks)
            chosenCap = nil
            chosenCoin = nil
        }
    }
}

/// How protecting what she holds is paid for with no coin on Bitcoin: one cashed out to herself, then.
let CASH_OUT_FIRST = "None of your bitcoin pays for this. Cash out this much to yourself, then " +
    "protect with it once it arrives. Beyond the cost and the reserve, it comes back to your balance."

/// Why no coin pays for a contract: each is spent whole, so short of the price or beyond what the largest takes.
let NO_FIT = "No contract of this hub takes a coin of yours whole: each is short of the price, " +
    "or beyond what the largest contract takes."

/// What protection asks of its owner, said before it is paid for: its end, `ends`, as a day.
func duty(ends: String) -> String { "Open Licash now and then, and before it ends on \(ends)." }

/// The blocks at `value` of a slider from `least` to `most`, in whole days but at its ends.
func blocksAt(value: Double, least: UInt32, most: UInt32) -> UInt32 {
    let days = max(Int64((value / Double(DAY_BLOCKS)).rounded()), 0) * DAY_BLOCKS
    return min(max(UInt32(truncatingIfNeeded: days), least), most)
}

/// Blocks in a day, about.
private let DAY_BLOCKS: Int64 = 144
