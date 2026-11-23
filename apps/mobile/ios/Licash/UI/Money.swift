import LicashCore
import SwiftUI

/// Where [Money] is `at`, for what is on Bitcoin: a hub's page is at its URL.
let ON_BITCOIN = "bitcoin"

/// Where the money is: how much is protected and how much is not, and what
/// is held at each hub and on Bitcoin. A row opens its page, `at` a hub's
/// URL or [ON_BITCOIN], through `go`: what is held there, and what to do
/// with it. `cashOut` pays out on Bitcoin what is held at a hub, as much as
/// the owner chooses, the amount suggested if any. `payOwn` pays
/// a link to an address of this wallet's, the address given.
struct Money: View {
    let engine: Engine
    let ui: Ui
    let at: String?
    let go: (String?) -> Void
    let cashOut: (_ hub: String, _ amount: UInt64?) -> Void
    let payOwn: (_ link: String, _ address: String) -> Void
    let back: () -> Void
    @SwiftUI.State private var holdings: [Holding]? = nil
    @SwiftUI.State private var notes: [Notes] = []
    @SwiftUI.State private var visited: [Holding] = []
    @SwiftUI.State private var error: String? = nil
    @SwiftUI.State private var changed = 0

    init(
        engine: Engine,
        ui: Ui,
        at: String?,
        go: @escaping (String?) -> Void,
        cashOut: @escaping (_ hub: String, _ amount: UInt64?) -> Void,
        payOwn: @escaping (_ link: String, _ address: String) -> Void,
        back: @escaping () -> Void
    ) {
        self.engine = engine
        self.ui = ui
        self.at = at
        self.go = go
        self.cashOut = cashOut
        self.payOwn = payOwn
        self.back = back
    }

    var body: some View {
        // A hub the wallet uses holds nothing until it is known to hold something.
        let held = ui.hubs.map { hub in
            (holdings ?? []).first { sameHub(a: $0.hub, b: hub.url) } ?? Holding(hub: hub.url, available: 0, pending: 0)
        } + (holdings ?? []).filter { h in !ui.hubs.contains { sameHub(a: $0.url, b: h.hub) } }
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                switch at {
                case nil:
                    TopBar(title: "Balance", back: back)
                case .some(ON_BITCOIN):
                    TopBar(title: "On Bitcoin", back: { go(nil) }) {
                        Info(title: "On Bitcoin", text: ON_BITCOIN_MEANS)
                    }
                case let at?:
                    TopBar(title: engine.called(url: at), back: { go(nil) })
                }
                if let error {
                    Text(error)
                        .textStyle(Theme.type.bodySmall)
                        .foregroundStyle(Theme.color.error)
                        .padding(.horizontal, 4)
                        .accessibilityIdentifier("error")
                }
                let fail = { (why: String) in error = why }
                switch at {
                case nil:
                    Summary(engine: engine, ui: ui, held: held, notes: notes, visited: visited, go: go) { changed += 1 }
                case .some(ON_BITCOIN):
                    OnBitcoin(engine: engine, ui: ui, go: go)
                case let at?:
                    HubPage(
                        engine: engine,
                        ui: ui,
                        holding: held.first { sameHub(a: $0.hub, b: at) } ?? Holding(hub: at, available: 0, pending: 0),
                        others: held,
                        notes: notes.filter { sameHub(a: $0.hub, b: at) },
                        cashOut: cashOut,
                        payOwn: payOwn,
                        fail: fail,
                        changed: { changed += 1 }
                    )
                }
                Spacer().frame(height: 48)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 20)
        }
        .task(id: [AnyHashable(ui.balance), AnyHashable(ui.history), AnyHashable(ui.hubs), AnyHashable(changed)]) {
            let all = await engine.holdings()
            // Replaced by a newer run, this one's answers are stale.
            if Task.isCancelled { return }
            switch all {
            case .success(let it): holdings = it
            case .failure(let e):
                holdings = []
                error = explain(e: e)
            }
            let noted = await engine.notes()
            if Task.isCancelled { return }
            switch noted {
            case .success(let it): notes = it
            case .failure(let e): error = explain(e: e)
            }
            let seen = await engine.visited()
            if Task.isCancelled { return }
            switch seen {
            case .success(let it): visited = it
            case .failure(let e): error = explain(e: e)
            }
        }
    }
}

/// Whether two URLs name one hub: contracts keep it without a last slash.
func sameHub(a: String, b: String) -> Bool {
    let trimmed = { (url: String) -> Substring in
        var s = Substring(url)
        while s.hasSuffix("/") { s.removeLast() }
        return s
    }
    return trimmed(a) == trimmed(b)
}

// Nested, as the Summary of Stats holds the name in the app.
extension Money {
    /// All of it, how much is protected, and where it is held.
    private struct Summary: View {
        let engine: Engine
        let ui: Ui
        let held: [Holding]
        let notes: [Notes]
        let visited: [Holding]
        let go: (String?) -> Void
        let changed: () -> Void
        // A hub only visited the owner asks about, by its URL.
        @SwiftUI.State private var adding: String? = nil
        @SwiftUI.State private var joining = false
        @SwiftUI.State private var joinError: String? = nil
        @SwiftUI.State private var joinWait = Wait()

        var body: some View {
            let colors = Theme.color
            let b = ui.balance
            let share = protection(balance: b)
            let feel = rememberFeel()
            VStack(alignment: .leading, spacing: 0) {
                Text(sat(amount: share.total))
                    .textStyle(Heading)
                    .padding(.horizontal, 4)
                    .padding(.vertical, 8)
                if share.total > 0 {
                    let safe = share.percent
                    GeometryReader { g in
                        HStack(spacing: 0) {
                            if safe > 0 { colors.primary.frame(width: g.size.width * Double(safe) / 100) }
                            if safe < 100 { colors.outline }
                        }
                    }
                    .frame(height: 10)
                    .background(colors.surfaceContainerHighest)
                    .clipShape(RoundedRectangle(cornerRadius: 5))
                    .frame(maxWidth: .infinity)
                    .padding(.top, 8)
                    .padding(.bottom, 4)
                    // Safe on Bitcoin apart from protected by contracts: bitcoin held
                    // protects notes once it pays for a contract.
                    let split = safeSplit(share: share, onBitcoin: b.onchain + b.moving + b.opening)
                    if split.bitcoin > 0 {
                        Share(icon: Glyphs.bitcoin, tint: colors.primary, label: "On Bitcoin", amount: split.bitcoin, tag: "safe-onchain", means: SAFE_ONCHAIN_MEANS)
                    }
                    Share(icon: Glyphs.shield, tint: colors.primary, label: PROTECTED_BY_CONTRACTS, amount: split.contracts, tag: "protected", means: PROTECTED_MEANS)
                    Share(icon: Glyphs.shieldOff, tint: colors.onSurfaceVariant, label: NOT_PROTECTED, amount: share.open, tag: "credit", means: NOT_PROTECTED_MEANS)
                    // Not while a contract is set up: it is that coin's being protected, as On Bitcoin says.
                    if share.open > 0 && b.onchain > 0 && ui.opening == nil {
                        Entry(
                            title: "Protect it with your bitcoin",
                            sub: "A contract paid with a coin on Bitcoin covers notes here",
                            tag: "protect-with-bitcoin",
                            onClick: { go(ON_BITCOIN) }
                        )
                    }
                }
                // In line with the total above it, not the groups' insides.
                Line(label: "Notes", value: digits(amount: noteCount(notes: notes)), tag: "note-count")
                    .padding(.horizontal, 4)
                Group(title: "") {
                    ForEach(held, id: \.hub) { holding in
                        // How much of it its contracts cover, and the notes it is in.
                        let cover = hubCover(hub: holding.hub, holding: holding, contracts: ui.contracts, notes: notes)
                        let sub = [cover.says, cover.notes > 0 ? cover.noteWords : nil].compactMap { $0 }.joined(separator: " · ")
                        Entry(
                            title: engine.called(url: holding.hub),
                            value: sat(amount: holding.available + holding.pending),
                            sub: sub.isEmpty ? nil : sub,
                            tag: "holding",
                            onClick: { go(holding.hub) }
                        )
                        CoverBar(cover: cover)
                            .padding(.bottom, 12)
                        Divider()
                    }
                    // Where bitcoin held is added to the balance: said, since nothing else leads here.
                    Entry(
                        title: "On Bitcoin",
                        value: sat(amount: b.onchain + b.moving + b.opening),
                        sub: b.onchain > 0 && b.moving == 0 && ui.opening == nil ? ADD_ONCHAIN : nil,
                        tag: "onchain",
                        onClick: { go(ON_BITCOIN) }
                    )
                }
                if !visited.isEmpty {
                    Group(title: "Not in your balance") {
                        ForEach(Array(visited.enumerated()), id: \.offset) { i, holding in
                            if i > 0 { Divider() }
                            Entry(
                                title: name(url: holding.hub),
                                value: sat(amount: holding.available + holding.pending),
                                tag: "visited",
                                onClick: { adding = holding.hub }
                            )
                        }
                    }
                }
            }
            .rememberWait(wait: joinWait, waiting: joining)
            .sheet(isPresented: Binding(get: { adding != nil }, set: { if !$0 { adding = nil; joinError = nil } })) {
                if let url = adding {
                    Sheet(title: name(url: url), dismiss: { adding = nil; joinError = nil }, dismissable: !joining) {
                        Say(text: "Left over from paying someone at this hub. Add the hub to use it.")
                        // Stays while the hub is added. Closed once it is, else why not under it.
                        BigButton(text: joining ? "Adding…" : "Add hub", busy: joining) {
                            joining = true
                            joinError = nil
                            Task {
                                switch await engine.addHub(url: url) {
                                case .success:
                                    adding = nil
                                    feel.confirm()
                                case .failure(let e):
                                    joinError = explain(e: e)
                                    feel.reject()
                                }
                                changed()
                                joining = false
                            }
                        }
                        .frame(maxWidth: .infinity)
                        .accessibilityIdentifier("add-visited")
                        Waited(wait: joinWait)
                        if let joinError { Problem(text: joinError) }
                    }
                }
            }
        }
    }
}

/// How bitcoin comes to be held, said while none is.
let NOTHING_ONCHAIN = "Receive at a Bitcoin address, then add it here"

/// What the On Bitcoin page does with bitcoin held, said under its row.
let ADD_ONCHAIN = "Add it to your balance, protected or not"

func noteCount(notes: [Notes]) -> UInt64 { notes.reduce(0) { $0 + $1.count } }

/// A share of the balance: its glyph and name, how much, and what it means on a tap.
private struct Share: View {
    let icon: ImageVector
    let tint: Color
    let label: String
    let amount: UInt64
    let tag: String
    let means: String

    var body: some View {
        HStack(alignment: .center, spacing: 0) {
            icon
                .resizable()
                .scaledToFit()
                .frame(width: 20, height: 20)
                .foregroundStyle(tint)
                .accessibilityHidden(true)
            Spacer().frame(width: 12)
            Text(label).textStyle(Theme.type.bodyLarge)
            Info(title: label, text: means)
            Spacer(minLength: 0)
            Text(sat(amount: amount))
                .textStyle(Amounts)
                .accessibilityIdentifier(tag)
        }
        .padding(.leading, 4)
        .frame(maxWidth: .infinity, minHeight: 48, alignment: .leading)
    }
}

/// What is held on Bitcoin, by where it stands, and what is on its way: to
/// protect, as offered ([Ui.offer]), or to add to the balance unprotected,
/// once warned.
private struct OnBitcoin: View {
    let engine: Engine
    let ui: Ui
    let go: (String?) -> Void
    @SwiftUI.State private var choosing = false
    @SwiftUI.State private var warned = false
    // Adding without protection, once confirmed: the sheet stays while it is done.
    @SwiftUI.State private var adding = false
    @SwiftUI.State private var addError: String? = nil
    @SwiftUI.State private var addWait = Wait()

    var body: some View {
        let b = ui.balance
        let coming = ui.incoming.reduce(0) { $0 + $1.amount }
        let feel = rememberFeel()
        // What each is called, how much, and its tag for the tests.
        let rows = [
            ("In your wallet", b.onchain, "kept"),
            ("Being added", b.moving, "moving"),
            ("Change coming back", b.opening, "change"),
            ("Arriving", coming, "coming"),
        ].filter { $0.1 > 0 }
        let offer = ui.opening == nil ? ui.offer : nil
        VStack(alignment: .leading, spacing: 0) {
            Text(sat(amount: b.onchain + b.moving + b.opening))
                .textStyle(Heading)
                .padding(.horizontal, 4)
                .padding(.vertical, 8)
            Group(title: "") {
                if rows.isEmpty { Entry(title: "Nothing here", sub: NOTHING_ONCHAIN) }
                ForEach(Array(rows.enumerated()), id: \.offset) { i, row in
                    if i > 0 { Divider() }
                    Line(label: row.0, value: sat(amount: row.1), tag: row.2)
                }
            }
            if offer != nil {
                Spacer().frame(height: 20)
                BigButton(text: "Add to balance, protected") { choosing = true }
                    .frame(maxWidth: .infinity)
                    .accessibilityIdentifier("offer-protect")
            } else if b.onchain > 0 && ui.opening == nil {
                // None offered, perhaps because it is too little for the price: the hub's own page
                // says what protecting costs and what it takes, hub by hub.
                let hubs = ui.hubs.filter { $0.contracts != nil }
                if !hubs.isEmpty {
                    Group(title: "Protect at") {
                        ForEach(Array(hubs.enumerated()), id: \.offset) { i, hub in
                            if i > 0 { Divider() }
                            Entry(title: engine.called(url: hub.url), tag: "protect-at", onClick: { go(hub.url) })
                        }
                    }
                }
            }
            if b.onchain > 0 && b.moving == 0 {
                Spacer().frame(height: offer != nil ? 8 : 20)
                BigButton(text: "Add without protection", quiet: true) { warned = true }
                    .frame(maxWidth: .infinity)
                    .accessibilityIdentifier("move-in")
            }
        }
        .rememberWait(wait: addWait, waiting: adding)
        .sheet(isPresented: Binding(
            get: {
                // As OfferSheet: none if no coin is taken whole by a contract within its hub's range.
                choosing && offer.map { q in
                    q.range.map { range in q.coins.contains { fitOf(q: q, range: range, blocks: q.suggestedBlocks, coin: $0) != nil } } == true
                } == true
            },
            set: { if !$0 { choosing = false } }
        )) {
            if let offer {
                OfferSheet(engine: engine, q: offer, ready: !behind(ui: ui)) { choosing = false }
            }
        }
        .sheet(isPresented: Binding(get: { warned }, set: { if !$0 { warned = false; addError = nil } })) {
            Sheet(title: "Not protected", dismiss: { warned = false; addError = nil }, dismissable: !adding) {
                Say(text: UNPROTECTED_MOVE)
                BigButton(text: adding ? "Adding…" : "Add anyway", danger: true, busy: adding) {
                    adding = true
                    addError = nil
                    Task {
                        switch await engine.moveLargest() {
                        case .success:
                            warned = false
                            feel.confirm()
                        case .failure(let e):
                            addError = explain(e: e)
                            feel.reject()
                        }
                        adding = false
                    }
                }
                .frame(maxWidth: .infinity)
                .accessibilityIdentifier("move-in-anyway")
                Waited(wait: addWait)
                if let addError { Problem(text: addError) }
            }
        }
    }
}

/// What adding Bitcoin to the balance with no protection risks.
let UNPROTECTED_MOVE = "In your balance, it is held at your hub: if the hub fails or " +
    "refuses to pay, it is lost. Protection recovers it on Bitcoin instead. Keep it here, or protect it first. " +
    "Your largest coin moves in whole, never joined with another."

/// What money on Bitcoin is, said on its page: safe already, but not spendable in Licash until added.
let ON_BITCOIN_MEANS = "Bitcoin you hold yourself: already safe, but paid only on Bitcoin, " +
    "slowly. Add it to your balance to pay in Licash: protected, through a contract at a hub that " +
    "covers it, or without protection, on the hub's word."

extension Engine {
    /// A hub of the wallet's, by the name it announced when added, else by its address.
    func called(url: String) -> String { hubName(url: url) ?? name(url: url) }
}
