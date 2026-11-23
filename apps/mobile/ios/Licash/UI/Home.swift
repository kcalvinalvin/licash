import LicashCore
import SwiftUI

/// Home, at a glance: what the wallet holds, how much of it is protected,
/// and what is going on, if anything. To receive and to send. The rest is a
/// tap away: where the money is behind the balance, the history behind
/// Activity, or a swipe up.
struct Home: View {
    let engine: Engine
    let ui: Ui
    let receive: () -> Void
    let send: () -> Void
    let settings: () -> Void
    let money: (_ at: String?) -> Void
    let history: () -> Void
    let open: (Activity) -> Void
    // How far the middle can still scroll to its end, and where it is on screen.
    @SwiftUI.State private var left: CGFloat = 0
    @SwiftUI.State private var middle: CGRect = .zero
    // Where the drag under way started, and how far the middle could still scroll then.
    @SwiftUI.State private var start: (at: CGPoint, left: CGFloat)? = nil

    var body: some View {
        // A swipe up opens the history, wherever it starts: what the middle,
        // which scrolls, leaves of one, and one over the rest.
        let far: CGFloat = 64
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .center, spacing: 0) {
                Wordmark()
                Spacer().frame(width: 10)
                NetworkChip(network: ui.network)
                Connections(engine: engine, ui: ui)
                Spacer(minLength: 0)
                IconButton(onClick: settings) { Image(systemName: "gearshape").accessibilityLabel("Settings") }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.top, 8)
            // In the middle of what is left, scrolling on short screens and with
            // large fonts. Pulled down, it looks for payments now.
            PullToLook(engine: engine) {
                GeometryReader { page in
                    ScrollView {
                        VStack(alignment: .center, spacing: 0) {
                            // Nothing works without a hub: choosing one comes first.
                            if ui.hubs.isEmpty {
                                ChooseHub(engine: engine, ui: ui)
                            } else {
                                Total(ui: ui) { money(nil) }
                                Going(ui: ui, money: money, history: history, open: open)
                                Offer(engine: engine, ui: ui)
                                Waiting(ui: ui, money: money)
                                Unprotected(engine: engine, ui: ui, money: money)
                                // A new wallet's first catching up: each step, as it goes.
                                if firstCatchingUp(ui: ui) {
                                    Spacer().frame(height: 16)
                                    VStack(alignment: .leading, spacing: 0) {
                                        CatchingUp(engine: engine, ui: ui)
                                        // Out of sight, iOS gives it some seconds, then decides when it runs again.
                                        Text("You can leave the app: it keeps catching up in the background, as iOS allows.")
                                            .textStyle(Theme.type.bodySmall)
                                            .foregroundStyle(Theme.color.onSurfaceVariant)
                                            .padding(.bottom, 8)
                                    }
                                    .padding(.horizontal, 16)
                                    .padding(.vertical, 8)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                                    .background(
                                        Theme.color.surfaceContainer,
                                        in: RoundedRectangle(cornerRadius: 16, style: .continuous)
                                    )
                                }
                            }
                        }
                        .frame(maxWidth: .infinity, minHeight: page.size.height)
                        .background(
                            GeometryReader { content in
                                // Below 0 once pulled past its end.
                                Color.clear.onChange(of: content.frame(in: .named("middle")).maxY - page.size.height, initial: true) { _, below in
                                    left = below
                                }
                                .onChange(of: page.frame(in: .global), initial: true) { _, frame in middle = frame }
                            }
                        )
                    }
                    .coordinateSpace(.named("middle"))
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            if !ui.hubs.isEmpty {
                HStack(alignment: .top, spacing: 12) {
                    BigButton(text: "Receive", onClick: receive)
                        .frame(maxWidth: .infinity)
                        .accessibilityIdentifier("receive")
                    BigButton(text: "Send", onClick: send)
                        .frame(maxWidth: .infinity)
                        .accessibilityIdentifier("send")
                }
                Button(action: history) {
                    VStack(alignment: .center, spacing: 0) {
                        Image(systemName: "chevron.up")
                            .resizable()
                            .scaledToFit()
                            .frame(width: 20, height: 20)
                            .foregroundStyle(Theme.color.onSurfaceVariant)
                        Text("Activity")
                            .textStyle(Theme.type.labelLarge)
                            .foregroundStyle(Theme.color.onSurfaceVariant)
                            .accessibilityIdentifier("activity")
                    }
                    .padding(.vertical, 8)
                    .frame(maxWidth: .infinity)
                    .contentShape(RoundedRectangle(cornerRadius: 16, style: .continuous))
                }
                .buttonStyle(.plain)
                .padding(.top, 8)
                .padding(.bottom, 8)
            }
        }
        .padding(.horizontal, 24)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .simultaneousGesture(
            DragGesture(coordinateSpace: .global)
                .onChanged { drag in
                    if start?.at != drag.startLocation { start = (drag.startLocation, left) }
                }
                .onEnded { drag in
                    // Over the middle, what it scrolls first does not count.
                    let scrolled = middle.contains(drag.startLocation) ? max(start?.left ?? 0, 0) : 0
                    if -drag.translation.height - scrolled > far { history() }
                }
        )
    }
}

/// What the wallet holds, and how much of it is protected: both open where it is.
private struct Total: View {
    let ui: Ui
    let open: () -> Void

    var body: some View {
        let colors = Theme.color
        let figure = digits(amount: ui.balance.total)
        Button(action: open) {
            HStack(alignment: .bottom, spacing: 0) {
                // Smaller once long, to stay on one line.
                let size = if figure.count <= 9 {
                    56
                } else if figure.count <= 11 {
                    46
                } else {
                    36
                }
                Text(figure)
                    .textStyle({ var style = Figures; style.size = CGFloat(size); return style }())
                    .lineLimit(1)
                    .accessibilityIdentifier("balance")
                Text(" sat")
                    .textStyle(Theme.type.titleMedium)
                    .foregroundStyle(colors.onSurfaceVariant)
                    .padding(.bottom, 8)
            }
            .padding(.horizontal, 8)
            .contentShape(RoundedRectangle(cornerRadius: 16, style: .continuous))
        }
        .buttonStyle(.plain)
        if let held = homeProtection(balance: ui.balance, opening: ui.opening != nil), let label = held.label {
            Spacer().frame(height: 12)
            Pill(
                icon: held.shield == Shield.none ? Glyphs.shieldOff : Glyphs.shield,
                text: label,
                // Calm, whichever: what is not protected is said, not shouted.
                tint: held.shield == Shield.all ? colors.primary : colors.onSurfaceVariant,
                tag: "protection",
                onClick: open
            )
        }
    }
}

/// What is going on, in a line: see ``glance(ui:now:)``.
private struct Going: View {
    let ui: Ui
    let money: (String?) -> Void
    let history: () -> Void
    let open: (Activity) -> Void
    @SwiftUI.State private var why: String? = nil

    var body: some View {
        if let going = glance(ui: ui, now: Int64(Date().timeIntervalSince1970)) {
            let busy = switch going {
            case .behind: false
            default: true
            }
            Spacer().frame(height: 8)
            Pill(
                icon: Image(systemName: "exclamationmark.triangle.fill"),
                text: going.text,
                busy: busy,
                tag: "status",
                onClick: {
                    switch going {
                    case let .one(_, activity, _): open(activity)
                    case .many: history()
                    case let .protecting(hub): money(hub)
                    case let .behind(reason): why = reason
                    }
                }
            )
            .sheet(isPresented: Binding(get: { why != nil }, set: { if !$0 { why = nil } })) {
                Sheet(title: "Not up to date", dismiss: { why = nil }) { Say(text: why ?? "") }
            }
        }
    }
}

/// The protection offered for what was paid on Bitcoin, to pay with it
/// once protected: until accepted, declined or being opened.
private struct Offer: View {
    let engine: Engine
    let ui: Ui
    @SwiftUI.State private var choosing = false

    var body: some View {
        if let q = ui.offer, !ui.offerDeclined, ui.opening == nil, let coin = q.coins.first {
            Spacer().frame(height: 8)
            Pill(
                icon: Glyphs.shield,
                // Already safe on Bitcoin: what it lacks is being spendable here.
                text: "Pay with your \(sat(amount: coin.value)) on Bitcoin",
                tint: Theme.color.primary,
                tag: "offer",
                onClick: { choosing = true }
            )
            .onChange(of: q) { choosing = false }
            .sheet(isPresented: $choosing) {
                OfferSheet(engine: engine, q: q, ready: !behind(ui: ui)) { choosing = false }
            }
        }
    }
}

/// Bitcoin paid to the wallet that no protection is offered for, nor any is
/// being set up: said, so it is not left to be found, and opening where it
/// is held and what to do with it.
private struct Waiting: View {
    let ui: Ui
    let money: (String?) -> Void

    var body: some View {
        if let coin = waiting(balance: ui.balance, offered: ui.offer != nil, opening: ui.opening != nil) {
            Spacer().frame(height: 8)
            Pill(
                icon: Glyphs.bitcoin,
                // Counted in the total already: what it lacks is being spendable, as the offer says.
                text: "Pay with your \(sat(amount: coin)) on Bitcoin",
                tag: "waiting",
                onClick: { money(ON_BITCOIN) }
            )
        }
    }
}

/// A hub holding more than its contracts protect, by at least its smallest
/// contract: said until dismissed on its page, which offers protecting it
/// or cashing part out.
private struct Unprotected: View {
    let engine: Engine
    let ui: Ui
    let money: (String?) -> Void
    @SwiftUI.State private var holdings: [Holding] = []

    var body: some View {
        let found = holdings.lazy.compactMap { h -> (hub: String, missing: UInt64)? in
            let cover = hubCover(hub: h.hub, holding: h, contracts: ui.contracts, notes: [])
            let terms = ui.hubs.first { sameHub(a: $0.url, b: h.hub) }
            return suggestProtecting(cover: cover, hub: terms, dismissed: engine.prefs.unprotectedDismissed(hub: h.hub))
                ? (h.hub, cover.missing) : nil
        }.first
        VStack(alignment: .center, spacing: 0) {
            if let found {
                Spacer().frame(height: 8)
                Pill(
                    icon: Glyphs.shieldOff,
                    text: "\(sat(amount: found.missing)) at \(name(url: found.hub)) not protected",
                    tag: "unprotected",
                    onClick: { money(found.hub) }
                )
            }
        }
        .task(id: [AnyHashable(ui.balance), AnyHashable(ui.contracts), AnyHashable(ui.hubs)]) {
            if case let .success(h) = await engine.holdings() { holdings = h }
        }
    }
}

/// No hub yet: those announced to pick from, looked for at once, or one by
/// the address its operator gave. What a hub is, once asked.
private struct ChooseHub: View {
    let engine: Engine
    let ui: Ui

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .center, spacing: 0) {
                Text("Choose a hub")
                    .textStyle(Theme.type.headlineSmall)
                    .accessibilityIdentifier("choose-hub")
                Info(title: "Hubs", text: HUB_MEANS)
            }
            .frame(maxWidth: .infinity)
            Group(title: "") { FoundHubs(engine: engine, ui: ui, auto: true) }
            Group(title: "Or its address") { AddHub(engine: engine, action: "Connect") }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
