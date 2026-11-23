import LicashCore
import SwiftUI

/// What is held at one hub, and what to do with it: cash it out, protect
/// it. Its protections, each opening on a tap. What the hub charges, once
/// asked. In advanced mode, the notes held there and moving them to
/// another hub too.
struct HubPage: View {
    let engine: Engine
    let ui: Ui
    let holding: Holding
    let others: [Holding]
    let notes: [Notes]
    let cashOut: (_ hub: String, _ amount: UInt64?) -> Void
    let payOwn: (_ link: String, _ address: String) -> Void
    let fail: (String) -> Void
    let changed: () -> Void
    // Protecting: the hub asked what it costs, then what it quotes.
    @SwiftUI.State private var quoting = false
    @SwiftUI.State private var quoteWait = Wait()
    @SwiftUI.State private var quote: ContractQuote? = nil
    @SwiftUI.State private var ending: Contract? = nil
    @SwiftUI.State private var forcing: Contract? = nil
    @SwiftUI.State private var shortfall: UInt64? = nil
    // A close of `ending` or `forcing`, asked and not done: the sheet may be left, it goes on.
    @SwiftUI.State private var closing = false
    @SwiftUI.State private var closeError: String? = nil
    @SwiftUI.State private var closeWait = Wait()
    @SwiftUI.State private var canceling: String? = nil
    @SwiftUI.State private var cancelingBusy = false
    @SwiftUI.State private var allowing: String? = nil
    @SwiftUI.State private var allowingBusy = false
    @SwiftUI.State private var fees = false
    @SwiftUI.State private var moving = false
    // Not protected and worth a contract: protect it or cash part out, until dismissed.
    @SwiftUI.State private var dismissed: UInt64? = nil

    init(
        engine: Engine,
        ui: Ui,
        holding: Holding,
        others: [Holding],
        notes: [Notes],
        cashOut: @escaping (_ hub: String, _ amount: UInt64?) -> Void,
        payOwn: @escaping (_ link: String, _ address: String) -> Void,
        fail: @escaping (String) -> Void,
        changed: @escaping () -> Void
    ) {
        self.engine = engine
        self.ui = ui
        self.holding = holding
        self.others = others
        self.notes = notes
        self.cashOut = cashOut
        self.payOwn = payOwn
        self.fail = fail
        self.changed = changed
    }

    private func protect() {
        quoting = true
        Task {
            switch await engine.contractQuote(hub: holding.hub) {
            case .success(let it): quote = it
            case .failure(let e): fail(explain(e: e))
            }
            quoting = false
        }
    }

    var body: some View {
        let hub = ui.hubs.first { sameHub(a: $0.url, b: holding.hub) }
        let now = Int64(Date().timeIntervalSince1970)
        // One closed with its hub still shows while its fold is awaited: her notes are out of her balance.
        let covers = ui.contracts.filter { c in
            sameHub(a: c.hub, b: holding.hub)
                && (ui.developer || shown(contract: c, now: now) || ui.recoveries.contains { r in r.id == c.id && carriedOn(recovery: r) })
        }
        let opening = ui.opening.map { sameHub(a: $0, b: holding.hub) } == true
        // One being set up shows the step it is at in its own row, once it has one.
        let stepping = covers.contains { $0.state == .opening && $0.step != nil }
        let held = holding.available + holding.pending
        let feel = rememberFeel()
        let cover = hubCover(hub: holding.hub, holding: holding, contracts: ui.contracts, notes: notes)
        let suggest = suggestProtecting(cover: cover, hub: hub, dismissed: dismissed)
        VStack(alignment: .leading, spacing: 0) {
            Text(sat(amount: holding.available))
                .textStyle(Heading)
                .padding(.horizontal, 4)
                .padding(.vertical, 8)
            if holding.pending > 0 {
                Line(label: "Settling", value: sat(amount: holding.pending))
                    .padding(.horizontal, 4)
            }
            // How much of it its contracts cover, what they leave out, the room they have left.
            if let says = cover.says {
                CoverBar(cover: cover)
                    .padding(.horizontal, 4)
                    .padding(.vertical, 4)
                Text("\(says) · \(cover.noteWords)")
                    .textStyle(Theme.type.bodySmall)
                    .foregroundStyle(Theme.color.onSurfaceVariant)
                    .padding(.horizontal, 4)
                    .accessibilityIdentifier("hub-cover")
            }
            Spacer().frame(height: 12)
            HStack(alignment: .top, spacing: 12) {
                BigButton(text: "Cash out", enabled: holding.available > 0, quiet: true) { cashOut(holding.hub, nil) }
                    .frame(maxWidth: .infinity)
                    .accessibilityIdentifier("cash-out")
                if hub?.contracts != nil && ui.opening == nil {
                    BigButton(text: quoting ? "Asking…" : "Protect", busy: quoting) { protect() }
                        .frame(maxWidth: .infinity)
                        .accessibilityIdentifier("open-contract")
                }
            }
            Waited(wait: quoteWait)
                .padding(.horizontal, 4)
                .padding(.vertical, 4)
            if suggest && ui.opening == nil {
                Group(title: "Not protected") {
                    Text(unprotectedWords(missing: cover.missing, hub: holding.hub))
                        .textStyle(Theme.type.bodyMedium)
                        .padding(.vertical, 12)
                        .accessibilityIdentifier("unprotected")
                    HStack(alignment: .top, spacing: 8) {
                        BigButton(text: quoting ? "Asking…" : "Protect", busy: quoting) { protect() }
                            .frame(maxWidth: .infinity)
                            .accessibilityIdentifier("suggest-protect")
                        BigButton(text: "Cash out part", quiet: true) {
                            cashOut(holding.hub, cover.missing)
                        }
                        .frame(maxWidth: .infinity)
                        .accessibilityIdentifier("suggest-cash-out")
                    }
                    .padding(.bottom, 12)
                    TextButton(onClick: {
                        engine.prefs.dismissUnprotected(hub: holding.hub, amount: cover.missing)
                        dismissed = cover.missing
                    }) { Text("Not now") }
                    .accessibilityIdentifier("suggest-dismiss")
                }
            }
            if !covers.isEmpty || opening {
                Group(title: "Protection") {
                    // Of what is held here, how much those running protect.
                    if covers.contains(where: { $0.state == .protecting }) {
                        Line(
                            label: "Protected now",
                            value: "\(digits(amount: covers.reduce(0) { $0 + $1.covers })) of \(sat(amount: held))",
                            tag: "hub-covered"
                        )
                        Divider()
                    }
                    ForEach(Array(covers.enumerated()), id: \.element.id) { i, c in
                        if i > 0 { Divider() }
                        CoverRow(
                            c: c,
                            height: ui.chain.height,
                            network: ui.network,
                            developer: ui.developer,
                            recovery: ui.recoveries.first { $0.id == c.id },
                            end: { ending = c },
                            forceClose: { forcing = c },
                            giveUp: { canceling = c.id },
                            allowFee: { allowing = c.id },
                            safety: { try? await engine.contractSafety(id: c.id).get() },
                            inspection: { try? await engine.contractInspection(id: c.id).get() },
                            explorer: { engine.explorerAddress(address: $0) }
                        )
                    }
                    if opening && !stepping {
                        if !covers.isEmpty { Divider() }
                        Entry(title: "Setting up", sub: SETUP_TAKES, tag: "opening")
                        LinearProgressIndicator()
                            .frame(maxWidth: .infinity)
                            .padding(.bottom, 12)
                    }
                }
            }
            Group(title: "") {
                Entry(title: "Fees and limits", onClick: { fees = true })
                if ui.developer && others.count >= 2 && holding.available > 0 {
                    Divider()
                    Entry(title: "Move to another hub", onClick: { moving = true })
                        .accessibilityIdentifier("move")
                }
            }
            if ui.developer {
                Group(title: "Notes") {
                    if notes.isEmpty { Entry(title: "None") }
                    ForEach(Array(notes.enumerated()), id: \.offset) { i, n in
                        if i > 0 { Divider() }
                        Line(label: "\(digits(amount: n.denomination)) sat × \(n.count)", value: sat(amount: n.denomination * n.count), tag: "denomination")
                    }
                }
            }
        }
        .rememberWait(wait: quoteWait, waiting: quoting)
        .rememberWait(wait: closeWait, waiting: closing)
        .onChange(of: holding.hub, initial: true) {
            dismissed = engine.prefs.unprotectedDismissed(hub: holding.hub)
        }
        .sheet(isPresented: Binding(get: { fees && hub != nil }, set: { if !$0 { fees = false } })) {
            if let hub { HubFees(hub: hub) { fees = false } }
        }
        .sheet(isPresented: Binding(get: { quote != nil }, set: { if !$0 { quote = nil } })) {
            if let q = quote {
                // Of terms chosen, if the hub lets them be: paid, whole, from a coin
                // held that a contract takes.
                if let range = q.range {
                    ChooseSheet(
                        q: q,
                        range: range,
                        coins: q.coins,
                        ready: !behind(ui: ui),
                        cover: cover,
                        open: { chosen, price in
                            quote = nil
                            engine.openContract(hub: q.hub, maxPrice: price, chosen: chosen)
                        },
                        dismiss: { quote = nil },
                        hub: engine.called(url: q.hub),
                        cashOut: { coin in
                            quote = nil
                            Task {
                                switch await engine.bitcoinAddress() {
                                case .success(let it): payOwn("bitcoin:\(it)?amount=\(btc(amount: coin))", it)
                                case .failure(let e): fail(explain(e: e))
                                }
                            }
                        }
                    )
                } else {
                    ProtectSheet(
                        q: q,
                        hub: engine.called(url: q.hub),
                        open: {
                            quote = nil
                            engine.openContract(hub: q.hub, maxPrice: q.price)
                        },
                        cashOutFirst: {
                            quote = nil
                            Task {
                                switch await engine.bitcoinAddress() {
                                case .success(let it): payOwn("bitcoin:\(it)?amount=\(btc(amount: q.price + FIXED_PART))", it)
                                case .failure(let e): fail(explain(e: e))
                                }
                            }
                        },
                        dismiss: { quote = nil }
                    )
                }
            }
        }
        .sheet(isPresented: Binding(get: { ending != nil }, set: { if !$0 { ending = nil; closeError = nil } })) {
            if let c = ending {
                EndSheet(
                    c: c,
                    height: ui.chain.height,
                    held: holding.available,
                    busy: closing,
                    error: closeError,
                    wait: closeWait,
                    cashOut: {
                        ending = nil
                        cashOut(c.hub, nil)
                    },
                    close: {
                        closing = true
                        closeError = nil
                        Task {
                            switch await engine.closeContract(id: c.id).value {
                            case .success:
                                ending = nil
                                feel.confirm()
                            case .failure(let e):
                                closeError = explain(e: e)
                                feel.reject()
                            }
                            closing = false
                        }
                    },
                    dismiss: {
                        ending = nil
                        closeError = nil
                    }
                )
            }
        }
        .sheet(isPresented: Binding(get: { forcing != nil }, set: { if !$0 { forcing = nil; shortfall = nil; closeError = nil } })) {
            if let c = forcing {
                ForceSheet(
                    c: c,
                    held: held,
                    shortfall: shortfall,
                    busy: closing,
                    error: closeError,
                    wait: closeWait,
                    force: {
                        closing = true
                        closeError = nil
                        Task {
                            switch await engine.forceClose(id: c.id).value {
                            case .success:
                                forcing = nil
                                shortfall = nil
                                feel.confirm()
                            case .failure(let e):
                                closeError = explain(e: e)
                                feel.reject()
                            }
                            closing = false
                        }
                    },
                    dismiss: {
                        forcing = nil
                        shortfall = nil
                        closeError = nil
                    }
                )
                .task(id: c.id) {
                    let it = try? await engine.challengeShortfall().get()
                    if !Task.isCancelled { shortfall = it }
                }
            }
        }
        .sheet(isPresented: Binding(get: { canceling != nil }, set: { if !$0 && !cancelingBusy { canceling = nil } })) {
            if let id = canceling {
                GiveUpSheet(
                    recovery: ui.recoveries.first { $0.id == id },
                    busy: cancelingBusy,
                    confirm: {
                        cancelingBusy = true
                        Task {
                            switch await engine.giveUpRecovery(id: id) {
                            case .success: canceling = nil
                            case .failure(let e):
                                canceling = nil
                                fail("Couldn't cancel closing: \(explain(e: e))")
                            }
                            cancelingBusy = false
                        }
                    },
                    dismiss: { if !cancelingBusy { canceling = nil } }
                )
            }
        }
        .sheet(isPresented: Binding(get: { allowing != nil }, set: { if !$0 && !allowingBusy { allowing = nil } })) {
            if let id = allowing {
                AllowFeeSheet(
                    recovery: ui.recoveries.first { $0.id == id },
                    busy: allowingBusy,
                    allow: { sats in
                        allowingBusy = true
                        Task {
                            switch await engine.allowPublicationFee(id: id, sats: sats) {
                            case .success: allowing = nil
                            case .failure(let e):
                                allowing = nil
                                fail("Couldn't allow the fee: \(explain(e: e))")
                            }
                            allowingBusy = false
                        }
                    },
                    dismiss: { if !allowingBusy { allowing = nil } }
                )
            }
        }
        .sheet(isPresented: $moving) {
            MoveSheet(
                engine: engine,
                from: holding,
                hubs: others,
                moved: {
                    moving = false
                    changed()
                },
                dismiss: { moving = false }
            )
        }
    }
}
