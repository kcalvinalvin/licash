import LicashCore
import SwiftUI

/*
 * Hubs, wherever one is chosen or looked at: those announced to pick
 * from, one added by its address, and what one charges. Shared by the
 * first run, which has none yet, and by Settings.
 */

/// What a hub is, said where one is chosen.
let HUB_MEANS = "A hub holds your balance and moves it for you. Up to what you protect, you can take it back " +
    "on Bitcoin without the hub. You can add more hubs later."

/// A hub in a line under its name: its fee on a payment, and what else it does.

/// A name a hub announced, as shown: its words one space apart, at most 40
/// characters, without control or formatting ones, none if blank. Its own
/// word only: its address goes with it.
func shownName(announced: String) -> String? {
    // No control or formatting characters, which could turn what follows around.
    let kept = announced.unicodeScalars.filter { c in
        // Tabs, line breaks and the information separators count as whitespace.
        let control = c.properties.generalCategory == .control && !(9...13).contains(c.value) && !(0x1C...0x1F).contains(c.value)
        return !control && c.properties.generalCategory != .format
    }
    let words = kept.split { " \t\n\u{0B}\u{0C}\r".unicodeScalars.contains($0) }
        .map { String(String.UnicodeScalarView($0)) }
    let cut = String(decoding: words.joined(separator: " ").utf16.prefix(40), as: UTF16.self)
    // Trimmed of the information separators too, whitespace as above.
    let shown = cut.trimmingCharacters(in: .whitespacesAndNewlines.union(CharacterSet(charactersIn: "\u{1C}"..."\u{1F}")))
    return shown.isEmpty ? nil : shown
}

func about(hub: Hub) -> String {
    let fee = paying(hub: hub)
    return [
        fee == "None" ? "No fee" : "Fee \(fee)",
        hub.lightning.map { _ in "Lightning" },
        hub.contracts.map { _ in "Protection" },
    ].compactMap { $0 }.joined(separator: " · ")
}

/// How many announced hubs did not answer, and whether Tor would reach them: nothing if all did.
func unanswered(found: HubsFound, tor: Bool) -> String? {
    if found.silent == 0 { return nil }
    let more = found.silent == 1 ? "1 more did not answer" : "\(found.silent) more did not answer"
    return !tor && found.silentOnion > 0 ? "\(more): try with Tor on" : more
}

/// What a hub charges and takes, in a sheet: only what it does. Its address,
/// to copy, `full`. What it `raised` since agreed, if anything, first, to
/// `approve`.
struct HubFees: View {
    let hub: Hub
    var full: Bool = false
    var raised: FeesRaised? = nil
    var approve: (FeesRaised) -> Void = { _ in }
    var optimize: (() -> Void)? = nil
    var approving: Bool = false
    var approveError: String? = nil
    var approveWait: Wait? = nil
    let dismiss: () -> Void
    @Environment(\.blockSeconds) private var blockSecs

    var body: some View {
        Sheet(title: full ? name(url: hub.url) : "Fees and limits", dismiss: dismiss, dismissable: !approving) {
            if full {
                Text(hub.url)
                    .fontDesign(.monospaced)
                    .textStyle(Theme.type.bodySmall)
                    .foregroundStyle(Theme.color.onSurfaceVariant)
                    .textSelection(.enabled)
                    .padding(.bottom, 8)
                    .accessibilityIdentifier("hub-url")
            }
            if let r = raised {
                Group(title: "Raised fees") {
                    ForEach(Array(raises(raised: r).enumerated()), id: \.offset) { i, raise in
                        if i > 0 { Divider() }
                        Entry(title: raise.what, value: raise.now, sub: "Was \(raise.agreed)", tag: "raised")
                    }
                }
                Problem(text: RAISED_MEANS, error: false)
                BigButton(text: approving ? "Approving…" : "Approve fees", busy: approving) { approve(r) }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .accessibilityIdentifier("approve-fees")
                Waited(wait: approveWait)
                if let approveError { Problem(text: approveError) }
            }
            // What a payment costs, a section a direction: limits under each.
            Group(title: "Paying") {
                Entry(title: "In Licash", value: paying(hub: hub), tag: "fee-pay")
                let most = hub.lightningPay
                if hub.lightning != nil || most != nil {
                    Divider()
                    Entry(
                        title: "Over Lightning",
                        value: hub.lightning.map { charge(rate: $0.pay) } ?? "Yes",
                        sub: most.map { "Up to \(sat(amount: $0))" }
                    )
                }
            }
            if hub.lightning != nil || hub.deposits != nil {
                Group(title: "Receiving") {
                    if let lightning = hub.lightning {
                        Entry(
                            title: "Over Lightning",
                            value: charge(rate: lightning.receive),
                            sub: between(min: lightning.receiveRange.min, max: lightning.receiveRange.max)
                        )
                    }
                    if let range = hub.deposits {
                        if hub.lightning != nil { Divider() }
                        Entry(
                            title: "On Bitcoin",
                            value: hub.depositFee.map { charge(rate: $0) } ?? "Yes",
                            sub: between(min: range.min, max: range.max)
                        )
                    }
                }
            }
            if let terms = hub.contracts {
                Group(title: "Protection") {
                    // Terms chosen within its range, priced by cap and length,
                    // else its one fixed contract.
                    if let range = terms.range {
                        // A row each, short enough not to be cut: the (i) on the
                        // protect sheet says what interest and the reserve are.
                        Entry(title: "Opening fee", value: sat(amount: range.fee), tag: "protection-price")
                        Divider()
                        Entry(title: "Interest", value: interestOf(range: range), tag: "protection-interest")
                        Divider()
                        Entry(title: "Reserve, refundable", value: sat(amount: range.reserve), tag: "protection-reserve")
                        Divider()
                        Entry(title: "Covers", value: between(min: range.minCap, max: range.maxCap), tag: "protection-cap")
                        Divider()
                        Entry(
                            title: "For",
                            value: "\(span(blocks: Int64(range.minBlocks), blockSecs: blockSecs)) to \(span(blocks: Int64(range.maxBlocks), blockSecs: blockSecs))",
                            tag: "protection-for"
                        )
                    } else {
                        Entry(title: "Price", value: feeOf(fee: terms.price, of: terms.cap))
                        Divider()
                        Entry(title: "Covers up to", value: sat(amount: terms.cap), tag: "protection-cap")
                        Divider()
                        Entry(title: "For", value: span(blocks: Int64(terms.blocks), blockSecs: blockSecs))
                    }
                }
            }
            if full {
                Group(title: "Held payments") {
                    Entry(title: "Holding one", value: charge(rate: hub.lockFee))
                    Divider()
                    Entry(title: "Refunding one", value: charge(rate: hub.refundFee))
                }
            }
            if let action = optimize {
                TextButton(onClick: action) { Text("Optimize notes") }
                    .accessibilityIdentifier("optimize-notes")
            }
        }
    }
}

/// Explicit optimization, with the shown fee as a hard execution cap.
struct OptimizeNotes: View {
    let engine: Engine
    let hub: Hub
    let dismiss: () -> Void
    @SwiftUI.State private var quote: NoteOptimization? = nil
    @SwiftUI.State private var loading = true
    @SwiftUI.State private var busy = false
    @SwiftUI.State private var error: String? = nil
    @SwiftUI.State private var wait = Wait()

    var body: some View {
        Sheet(title: "Optimize notes", dismiss: dismiss, dismissable: !busy) {
            Say(text: "Smaller notes leave receiving room and help recovery fit its note limit. Automatic shaping normally handles this.")
            Say(text: "Doing it manually bypasses privacy rest and the daily fee budget, but not the fees you agreed to.")
            if loading {
                Say(text: "Checking the notes…")
            } else if quote == nil && error == nil {
                Say(text: "No optimization is available, or one is still pending.")
            } else if let q = quote {
                Line(label: "Notes", value: "\(q.notesBefore) to \(q.notesAfter)")
                Line(label: "Reminting", value: sat(amount: q.value))
                Line(label: "Fee", value: feeOf(fee: q.fee, of: q.value))
                if q.id != nil {
                    Say(text: "Optimized. Its fee is recorded in History.")
                } else {
                    BigButton(text: "Optimize for \(sat(amount: q.fee))", enabled: !busy) {
                        busy = true
                        error = nil
                        Task {
                            switch await engine.optimizeNotes(quote: q) {
                            case .success(let done): quote = done
                            case .failure(let e): error = explain(e: e)
                            }
                            busy = false
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .accessibilityIdentifier("confirm-optimize")
                }
            }
            Waited(wait: wait)
            if let error { Problem(text: error) }
            TextButton(onClick: dismiss, enabled: !busy) { Text("Done") }
        }
        .rememberWait(wait: wait, waiting: loading || busy)
        .task(id: hub.url) {
            quote = nil
            loading = true
            switch await engine.optimizationQuote(hub: hub.url) {
            case .success(let q): quote = q
            case .failure(let e): error = explain(e: e)
            }
            loading = false
        }
    }
}

/// A hub's interest by the year, on its own part of the cover, then on hers if it charges any.
func interestOf(range: ContractRange) -> String {
    "\(percent(ppm: UInt64(range.ppmYear))) a year" +
        (range.ppmYearFunded == 0 ? "" : ", \(percent(ppm: UInt64(range.ppmYearFunded))) on yours")
}

/// From `min` to `max`, in sat: a range a hub takes.
private func between(min: UInt64, max: UInt64) -> String { "\(digits(amount: min)) to \(sat(amount: max))" }

/// A hub added by its address, typed or pasted: `action` names the button.
struct AddHub: View {
    let engine: Engine
    var action: String = "Add"
    @SwiftUI.State private var url = ""
    @SwiftUI.State private var adding = false
    @SwiftUI.State private var error: String? = nil
    @SwiftUI.State private var wait = Wait()

    var body: some View {
        let feel = rememberFeel()
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .center, spacing: 12) {
                OutlinedTextField(value: url, onValueChange: { url = $0 }, label: "Hub address", singleLine: true)
                    .keyboardType(.URL)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .submitLabel(.done)
                    .frame(maxWidth: .infinity)
                    .accessibilityIdentifier("hub-address")
                FilledTonalButton(
                    onClick: {
                        adding = true
                        Task {
                            switch await engine.addHub(url: url.trimmingCharacters(in: .whitespacesAndNewlines)) {
                            case .success:
                                url = ""
                                error = nil
                                feel.confirm()
                            case .failure(let e):
                                error = explain(e: e)
                                feel.reject()
                            }
                            adding = false
                        }
                    },
                    enabled: !url.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && !adding
                ) { Text(adding ? "…" : action) }
                    .accessibilityIdentifier("add-address")
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.vertical, 12)
            Waited(wait: wait)
            if let error { Problem(text: error) }
        }
        .rememberWait(wait: wait, waiting: adding)
    }
}

/// The hubs announced on Nostr, on the relays the hubs used name and on the
/// default ones, to pick from: a tap shows one, and adds it once the owner
/// says so. Looked for at once if `auto`, else once asked. Those announced
/// but not answering are only counted.
struct FoundHubs: View {
    let engine: Engine
    let ui: Ui
    let auto: Bool
    @SwiftUI.State private var found: HubsFound? = nil
    @SwiftUI.State private var looking = false
    @SwiftUI.State private var error: String? = nil
    // The one picked, in its sheet, until it is added, `joining` while it is.
    @SwiftUI.State private var adding: FoundHub? = nil
    @SwiftUI.State private var joining = false
    @SwiftUI.State private var joinError: String? = nil
    // The last added, said under the list.
    @SwiftUI.State private var joined: String? = nil
    @SwiftUI.State private var lookWait = Wait()
    @SwiftUI.State private var joinWait = Wait()

    private func look() {
        looking = true
        error = nil
        Task {
            switch await engine.findHubs() {
            case .success(let f): found = f
            case .failure(let e): error = explain(e: e)
            }
            looking = false
        }
    }

    var body: some View {
        let feel = rememberFeel()
        // Those added since looking drop out.
        let shown = found?.hubs.filter { f in !ui.hubs.contains { sameHub(a: $0.url, b: f.hub.url) } }
        VStack(alignment: .leading, spacing: 0) {
            ForEach(shown ?? [], id: \.hub.url) { f in
                Entry(title: f.name ?? name(url: f.hub.url), sub: about(hub: f.hub), onClick: { adding = f })
                    .accessibilityIdentifier("add-found")
                Divider()
            }
            if shown?.isEmpty == true {
                Entry(title: "None found", tag: "none-found")
                if let none = noneFound(network: ui.network, none: ui.hubs.isEmpty) { Problem(text: none, error: false) }
            }
            if let silent = found.flatMap({ unanswered(found: $0, tor: ui.tor) }) { Problem(text: silent, error: false) }
            if let error { Problem(text: error) }
            TextButton(onClick: look, enabled: !looking) {
                Text(looking ? "Looking…" : found == nil ? "Find hubs" : "Look again")
            }
            .accessibilityIdentifier("find-hubs")
            Waited(wait: lookWait)
                .padding(.leading, 12)
            if let joined {
                Text("Added \(joined)")
                    .textStyle(Theme.type.bodyMedium)
                    .padding(.leading, 12)
                    .padding(.top, 8)
                    .accessibilityIdentifier("joined")
            }
        }
        .rememberWait(wait: lookWait, waiting: looking)
        .rememberWait(wait: joinWait, waiting: joining)
        .task { if auto { look() } }
        .sheet(isPresented: Binding(get: { adding != nil }, set: { if !$0 { adding = nil; joinError = nil } })) {
            if let f = adding {
                let named = f.name ?? name(url: f.hub.url)
                Sheet(title: named, dismiss: { adding = nil; joinError = nil }, dismissable: !joining) {
                    Text(f.hub.url)
                        .fontDesign(.monospaced)
                        .textStyle(Theme.type.bodySmall)
                        .foregroundStyle(Theme.color.onSurfaceVariant)
                        .padding(.bottom, 8)
                        .accessibilityIdentifier("found-url")
                    Line(label: "Paying", value: paying(hub: f.hub))
                    Say(text: "Up to what you protect, your balance here can be taken back on Bitcoin without the hub. Above it, it rests on the hub's word.")
                    // Stays here while it is added, which asks the hub a few times:
                    // closed once it is, or why not said under the button.
                    BigButton(text: joining ? "Adding…" : "Add hub", busy: joining) {
                        joining = true
                        joinError = nil
                        Task {
                            switch await engine.addHub(url: f.hub.url, announced: f.name) {
                            case .success:
                                joined = named
                                adding = nil
                                feel.confirm()
                            case .failure(let e):
                                joinError = explain(e: e)
                                feel.reject()
                            }
                            joining = false
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .accessibilityIdentifier("add-hub")
                    Waited(wait: joinWait)
                    if let joinError { Problem(text: joinError) }
                }
            }
        }
    }
}

/// Why no hub was found, on a `network` where none may run yet, for a wallet
/// with `none` yet: nil where nothing more is to be said.
func noneFound(network: Network, none: Bool) -> String? {
    network == .bitcoin && none ? NONE_ON_BITCOIN : nil
}

/// No hub on Bitcoin yet: what to do to try it.
let NONE_ON_BITCOIN = "Licash is in testing: hubs run on test networks only for now. " +
    "To try it, delete this wallet (Settings, Advanced mode, Advanced) and make one with \"Use a test network\"."

/// Something that went wrong, or to know, in a line under what it is about.
struct Problem: View {
    let text: String
    var error: Bool = true

    var body: some View {
        Text(text)
            .textStyle(Theme.type.bodySmall)
            .foregroundStyle(error ? Theme.color.error : Theme.color.onSurfaceVariant)
            .padding(.top, 8)
            .padding(.bottom, 12)
    }
}
