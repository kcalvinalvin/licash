import CoreTransferable
import LicashCore
import SwiftUI
import UniformTypeIdentifiers

/// A page of Settings, behind a row of its first.
private enum Page: String {
    case hubs = "Hubs"
    case backup = "Backup"
    case cold = "Cold storage"
    case bitcoin = "Bitcoin"
    case tor = "Tor"
    case about = "About"
    case advanced = "Advanced"

    var title: String { rawValue }
}

private let TOR_MEANS =
    "Reaches hubs, relays and Bitcoin through Tor, each apart: slower, and hides where you are from them."

private let ADVANCED_MEANS =
    "Shows how payments are made, the notes held, providers to choose from, " +
    "the Bitcoin light client's settings and the log."

/// Settings: on its first page, a row for each kind of thing, each with a
/// glyph on a tile. What is on or off switches there, what holds more
/// opens a page of its own.
struct Settings: View {
    let engine: Engine
    let ui: Ui
    let balance: () -> Void
    let back: () -> Void
    // A page goes back to the first through its own back button: iOS has no
    // system back.
    @SwiftUI.State private var page: Page? = nil

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                switch page {
                case nil:
                    TopBar(title: "Settings", back: back)
                    Overview(engine: engine, ui: ui) { page = $0 }
                case let p?:
                    TopBar(title: p.title, back: { page = nil })
                    switch p {
                    case .hubs: HubsPage(engine: engine, ui: ui)
                    case .backup: BackupPage(engine: engine, ui: ui)
                    case .cold: ColdPage(engine: engine, ui: ui)
                    case .bitcoin: BitcoinPage(engine: engine, ui: ui)
                    case .tor: TorPage(engine: engine, ui: ui)
                    case .about: About(ui: ui)
                    case .advanced: Advanced(engine: engine, ui: ui, balance: balance)
                    }
                }
                Spacer().frame(height: 48)
            }
            .padding(.horizontal, 20)
        }
    }
}

/// The first page: hubs, Bitcoin and Tor, about the app, advanced mode.
private struct Overview: View {
    let engine: Engine
    let ui: Ui
    let open: (Page) -> Void
    @SwiftUI.State private var torFailed: String? = nil

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Group(title: "") {
                Entry(title: "Hubs", value: "\(ui.hubs.count)", icon: Glyphs.bank, tag: "hubs") { open(.hubs) }
                if ui.wallet {
                    Divider()
                    Entry(title: "Backup", icon: Glyphs.key, tag: "backup") { open(.backup) }
                }
                // An advanced feature: shown in advanced mode, or once set up.
                let cold = ui.cold
                if ui.wallet && (ui.developer || cold != nil) {
                    Divider()
                    Entry(
                        title: "Cold storage",
                        value: cold.map { sat(amount: $0.balance) } ?? "Off",
                        sub: cold.map { "Withdraws past \(sat(amount: $0.threshold))" },
                        icon: Image(systemName: "lock.fill"),
                        tag: "cold"
                    ) { open(.cold) }
                }
            }
            Group(title: "") {
                let (state, detail) = status(ui: ui, developer: ui.developer)
                Entry(title: "Bitcoin", value: state, sub: detail, icon: Glyphs.bitcoin, tag: "chain") { open(.bitcoin) }
                if let problem = ui.problem { Problem(text: problem) }
                Divider()
                Toggle(title: "Tor", on: ui.tor, sub: "Hides where you are, slower", icon: Image(systemName: "lock.fill"), means: TOR_MEANS) { on in
                    torFailed = nil
                    Task { if case .failure(let e) = await engine.setTor(on: on) { torFailed = explain(e: e) } }
                }
                if let torFailed { Problem(text: torFailed) }
                if ui.tor {
                    Divider()
                    let value = switch ui.torStatus {
                    case nil: "Starting"
                    case let tor? where tor.ready: "Connected"
                    case let tor? where tor.blocked != nil: "Stuck"
                    case let tor?: "\(tor.percent)%"
                    }
                    Entry(title: "Tor status", value: value, tag: "tor-status") { open(.tor) }
                }
            }
            Group(title: "") {
                Entry(title: "About", icon: Image(systemName: "info.circle"), tag: "about") { open(.about) }
            }
            Group(title: "") {
                Toggle(title: "Advanced mode", on: ui.developer, icon: Glyphs.code, means: ADVANCED_MEANS) { engine.setDeveloper(on: $0) }
                if ui.developer {
                    Divider()
                    Entry(title: "Advanced", icon: Image(systemName: "wrench.and.screwdriver"), tag: "advanced") { open(.advanced) }
                }
            }
        }
    }
}

/// The hubs used, each opening what it charges, one added by its address, or found.
private struct HubsPage: View {
    let engine: Engine
    let ui: Ui
    @SwiftUI.State private var shown: Hub? = nil
    @SwiftUI.State private var optimizing: Hub? = nil
    // Approving a hub's fees: its sheet stays while it is done.
    @SwiftUI.State private var approving = false
    @SwiftUI.State private var approveError: String? = nil
    @SwiftUI.State private var approveWait = Wait()

    private func raised(hub: Hub) -> FeesRaised? { ui.raised.first { $0.hub == hub.url } }

    var body: some View {
        let feel = rememberFeel()
        VStack(alignment: .leading, spacing: 0) {
            if !ui.hubs.isEmpty {
                Group(title: "") {
                    ForEach(Array(ui.hubs.enumerated()), id: \.offset) { i, hub in
                        if i > 0 { Divider() }
                        // Named as it announced itself, its address under it, else by its address.
                        let announced = engine.hubName(url: hub.url)
                        let sub = if raised(hub: hub) != nil {
                            "Raised its fees: approve to pay there"
                        } else if announced != nil {
                            name(url: hub.url)
                        } else {
                            about(hub: hub)
                        }
                        Entry(title: announced ?? name(url: hub.url), sub: sub, onClick: { shown = hub })
                            .accessibilityIdentifier("hub")
                    }
                }
            }
            Group(title: "Add a hub") { AddHub(engine: engine) }
            Group(title: "More hubs") { FoundHubs(engine: engine, ui: ui, auto: false) }
        }
        .rememberWait(wait: approveWait, waiting: approving)
        .sheet(isPresented: Binding(get: { shown != nil }, set: { if !$0 { shown = nil; approveError = nil } })) {
            if let hub = shown {
                HubFees(
                    hub: hub,
                    full: true,
                    raised: raised(hub: hub),
                    approve: { r in
                        approving = true
                        approveError = nil
                        Task {
                            switch await engine.approveFees(raised: r) {
                            case .success:
                                shown = nil
                                feel.confirm()
                            case .failure(let e):
                                approveError = explain(e: e)
                                feel.reject()
                            }
                            approving = false
                        }
                    },
                    optimize: ui.developer ? { shown = nil; optimizing = hub } : nil,
                    approving: approving,
                    approveError: approveError,
                    approveWait: approveWait
                ) {
                    shown = nil
                    approveError = nil
                }
            }
        }
        .sheet(isPresented: Binding(get: { optimizing != nil }, set: { if !$0 { optimizing = nil } })) {
            if let hub = optimizing { OptimizeNotes(engine: engine, hub: hub) { optimizing = nil } }
        }
    }
}

/// The app's version, what it keeps on the phone, and its logs to share.
private struct About: View {
    let ui: Ui
    @SwiftUI.State private var kept: Storage? = nil
    @SwiftUI.State private var sizes = false

    var body: some View {
        let version = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? ""
        let commit = Bundle.main.object(forInfoDictionaryKey: "LicashCommit") as? String ?? "unknown"
        Group(title: "") {
            Line(label: "Version", value: "\(version) (\(commit))", tag: "version")
            Divider()
            Entry(title: "Storage", value: kept.map { bytes(bytes: $0.total) } ?? "…", tag: "storage") {
                // Opened once measured, if tapped before.
                sizes = true
            }
            Divider()
            ShareLink(
                item: LogsFile(ui: ui),
                subject: Text("Licash logs"),
                preview: SharePreview("Licash logs")
            ) {
                HStack(alignment: .center, spacing: 4) {
                    Entry(title: "Export logs", tag: "export-logs")
                    Chevron()
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
        }
        .task { kept = await Task.detached { storage() }.value }
        .sheet(isPresented: Binding(get: { sizes && kept != nil }, set: { if !$0 { sizes = false } })) {
            if let k = kept {
                Sheet(title: "Storage", dismiss: { sizes = false }) { StorageUsed(k: k, advanced: ui.developer) }
            }
        }
    }
}

/// The logs, written to one file as a share asks for them.
private struct LogsFile: Transferable {
    let ui: Ui

    static var transferRepresentation: some TransferRepresentation {
        FileRepresentation(exportedContentType: .plainText) { SentTransferredFile(exportLogs(ui: $0.ui)) }
    }
}

/// What the app keeps on the phone, by what keeps it, in advanced mode, each network's light client.
private struct StorageUsed: View {
    let k: Storage
    let advanced: Bool

    var body: some View {
        let size = { (b: Int64) in bytes(bytes: b) }
        VStack(alignment: .leading, spacing: 0) {
            Line(label: "Total", value: size(k.total), tag: "storage-total")
            Divider()
            Line(label: "Wallet", value: size(k.wallet))
            Line(label: "Bitcoin light client", value: size(k.chain), tag: "storage-chain")
            if advanced {
                ForEach(k.chains, id: \.0) { chain in Line(label: "  \(chain.0)", value: size(chain.1)) }
            }
            if k.tor > 0 { Line(label: "Tor", value: size(k.tor)) }
            Line(label: "Logs", value: size(k.logs))
            Line(label: "Cache", value: size(k.cache))
            if k.other > 0 { Line(label: "Other", value: size(k.other)) }
            if let app = k.app { Line(label: "The app itself", value: size(app), tag: "storage-app") }
        }
    }
}

/// `bytes` said short: the next unit up past 900 of one, in steps of 1,000,
/// with two decimals below 1, one below 10, else none, rounded half up.
/// Worked in Float, so 1,450 B reads 1.5 kB.
private func bytes(bytes: Int64) -> String {
    let units = ["B", "kB", "MB", "GB", "TB", "PB"]
    var size = Float(bytes)
    var unit = 0
    while size > 900 && unit < units.count - 1 {
        size /= 1000
        unit += 1
    }
    let decimals = unit == 0 || size >= 100 ? 0 : size < 1 ? 2 : size < 10 ? 1 : 0
    let f = NumberFormatter()
    f.numberStyle = .decimal
    f.usesGroupingSeparator = false
    f.roundingMode = .halfUp
    f.minimumFractionDigits = decimals
    f.maximumFractionDigits = decimals
    return "\(f.string(from: NSNumber(value: size)) ?? "") \(units[unit])"
}

/// How the app catches up with Bitcoin, step by step, and the block the
/// wallet's payments are looked for from.
private struct BitcoinPage: View {
    let engine: Engine
    let ui: Ui
    @SwiftUI.State private var birthday: UInt32? = nil

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Group(title: "") { SyncStages(engine: engine, ui: ui) }
            if let problem = ui.problem { Problem(text: problem) }
            Group(title: "") {
                Line(label: "Network", value: named(network: ui.network))
                Divider()
                Line(label: "Wallet birthday", value: birthday.map { "Block \(digits(amount: UInt64($0)))" } ?? "Not known yet", tag: "birthday")
                if let scan = ui.scan {
                    Divider()
                    Line(label: "Payments looked for up to", value: "Block \(digits(amount: UInt64(max(Int64(scan.next) - 1, 0))))")
                }
            }
        }
        .task(id: ui.scan?.from) { birthday = try? await engine.birthday().get() }
    }
}

/// How the app's Tor client stands: how far it is in connecting, what
/// it is doing, and why it seems stuck, and starting it afresh, after a yes.
private struct TorPage: View {
    let engine: Engine
    let ui: Ui
    @SwiftUI.State private var asking = false
    @SwiftUI.State private var busy = false
    @SwiftUI.State private var error: String? = nil

    var body: some View {
        let tor = ui.torStatus
        VStack(alignment: .leading, spacing: 0) {
            Group(title: "") {
                if !ui.tor {
                    Line(label: "Status", value: "Off")
                } else {
                    Line(label: "Status", value: tor?.ready == true ? "Connected" : "Connecting", tag: "tor-state")
                    Divider()
                    Line(label: "Progress", value: tor.map { "\($0.percent)%" } ?? "…", tag: "tor-percent")
                    if let tor {
                        if !tor.ready {
                            LinearProgressIndicator(progress: Double(tor.percent) / 100)
                                .frame(maxWidth: .infinity, alignment: .leading)
                        }
                        Divider()
                        Text(tor.summary)
                            .textStyle(Theme.type.bodyMedium)
                            .foregroundStyle(Theme.color.onSurfaceVariant)
                            .padding(.vertical, 12)
                            .accessibilityIdentifier("tor-summary")
                        if let why = tor.blocked { Problem(text: "Stuck: \(why)") }
                    }
                }
            }
            if ui.tor {
                Group(title: "") {
                    Entry(title: "Reset Tor", sub: "Forget what Tor learned and connect afresh", tag: "reset-tor") {
                        error = nil
                        asking = true
                    }
                    if let error { Problem(text: error) }
                }
            }
        }
        .task {
            while !Task.isCancelled {
                await engine.live()
                try? await Task.sleep(for: .seconds(2))
            }
        }
        .sheet(isPresented: $asking) {
            Sheet(title: "Reset Tor?", dismiss: { asking = false }, dismissable: !busy) {
                Say(text: "Forgets what Tor learned of its network and connects from the start. Use it when Tor does not connect. Nothing you hold is touched.")
                BigButton(text: busy ? "Resetting…" : "Reset", enabled: !busy) {
                    busy = true
                    Task {
                        if case .failure(let e) = await engine.resetTor() { error = explain(e: e) }
                        busy = false
                        asking = false
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .accessibilityIdentifier("reset-tor-confirm")
                Spacer().frame(height: 12)
                BigButton(text: "Cancel", enabled: !busy, quiet: true) { asking = false }
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }
}

/// How far the app follows Bitcoin, in a word, and what there is to add:
/// in advanced mode, the block it is at and the nodes it hears from.
func status(ui: Ui, developer: Bool) -> (String, String?) {
    let chain = ui.chain
    let nodes = "\(chain.peers) " + (chain.peers == 1 ? "node" : "nodes")
    // The pinned headers come first, from no node: said as on Home.
    if (0...99).contains(chain.importing) { return ("Downloading", nil) }
    if chain.importing >= 100 { return ("Setting up", nil) }
    if chain.peers == 0 { return ("Connecting", nil) }
    if !chain.synced { return ("Catching up", developer ? "Block \(chain.height), \(nodes)" : nil) }
    if let scan = ui.scan, Int(scan.next) <= chain.height {
        return ("Checking", developer ? "Block \(scan.next) of \(chain.height)" : nil)
    }
    return ("Up to date", developer ? "Block \(chain.height), \(nodes)" : nil)
}

/// For those testing the app: the balance's notes, the log, what renewing
/// notes may cost, the light client's settings, and deleting the wallet.
private struct Advanced: View {
    let engine: Engine
    let ui: Ui
    let balance: () -> Void
    @SwiftUI.State private var logs = false
    @SwiftUI.State private var deleting = false
    @SwiftUI.State private var erasing = false
    @SwiftUI.State private var eraseError: String? = nil
    @SwiftUI.State private var eraseWait = Wait()

    var body: some View {
        let feel = rememberFeel()
        VStack(alignment: .leading, spacing: 0) {
            Group(title: "") {
                Entry(title: "Balance and notes", onClick: balance)
                Divider()
                Toggle(title: "Show the log", on: logs) { logs = $0 }
                if logs {
                    Text(AppLog.recent().suffix(80).joined(separator: "\n"))
                        .font(.system(size: 10, design: .monospaced))
                        .padding(.bottom, 12)
                }
            }
            if ui.wallet { Group(title: "Renewing notes") { RenewLimit(engine: engine) } }
            if ui.wallet {
                Group(title: "Recovery safety") {
                    ExitMargin(engine: engine)
                    Divider()
                    ExitConsentEntry(engine: engine, contracts: !ui.contracts.isEmpty)
                }
            }
            Group(title: "Bitcoin light client") {
                LightClient(engine: engine)
                Divider()
                ResetChain(engine: engine)
            }
            Group(title: "") {
                Entry(title: "Delete wallet", tag: "delete-wallet", color: Theme.color.error) { deleting = true }
            }
        }
        .rememberWait(wait: eraseWait, waiting: erasing)
        .sheet(isPresented: Binding(get: { deleting }, set: { if !$0 { deleting = false; eraseError = nil } })) {
            Sheet(title: "Delete this wallet?", dismiss: { deleting = false; eraseError = nil }, dismissable: !erasing) {
                Say(text: "What it holds is lost for good, unless you saved a backup file.")
                // Stays while it is done: then the first screen shows in its place.
                BigButton(text: erasing ? "Deleting…" : "Delete", danger: true, busy: erasing) {
                    erasing = true
                    eraseError = nil
                    Task {
                        switch await engine.delete() {
                        case .success: deleting = false
                        case .failure(let e):
                            eraseError = explain(e: e)
                            feel.reject()
                        }
                        erasing = false
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .accessibilityIdentifier("delete")
                Waited(wait: eraseWait)
                if let eraseError { Problem(text: eraseError) }
                Spacer().frame(height: 12)
                BigButton(text: "Cancel", enabled: !erasing, quiet: true) { deleting = false; eraseError = nil }
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }
}

/// The most the wallet pays, on its own, to renew notes a hub that
/// restarted can no longer unlink: in parts per million of them.
private struct RenewLimit: View {
    let engine: Engine
    @SwiftUI.State private var ppm = ""
    @SwiftUI.State private var error: String? = nil
    @SwiftUI.State private var saved = false

    var body: some View {
        let typed = ppm.trimmingCharacters(in: .whitespacesAndNewlines)
        let asked = UInt32(typed: typed)
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .center, spacing: 0) {
                Text("Most to pay").textStyle(Theme.type.bodyLarge)
                Info(
                    title: "Renewing notes",
                    text: "A hub that restarted can no longer unlink the notes it issued before. " +
                        "The wallet swaps them for new ones there within this share, or if its fees are at most " +
                        "1 sat base, this rate and 0.1 sat per output. Protection's agreed fees take precedence. 0 means free only."
                )
            }
            HStack(alignment: .center, spacing: 12) {
                Field(
                    value: ppm,
                    change: { ppm = $0; error = nil; saved = false },
                    label: "Parts per million",
                    hint: error ?? asked.map { "\(percent(ppm: UInt64($0))) of the notes renewed" } ?? "0 for free renewals only",
                    failed: error != nil || (!typed.isEmpty && asked == nil),
                    number: true
                )
                FilledTonalButton(
                    onClick: {
                        if let asked {
                            Task {
                                switch await engine.setRenewLimit(ppm: asked) {
                                case .success: saved = true
                                case .failure(let e): error = explain(e: e)
                                }
                            }
                        }
                    },
                    enabled: asked != nil && !saved
                ) { Text(saved ? "Saved" : "Save") }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.bottom, 8)
        }
        // Read once, as it shows: SwiftUI makes this view again on each update of the page.
        .onAppear { ppm = engine.renewLimit().map { String($0) } ?? "" }
    }
}

/// A holder-owned planning margin: what exits plan with once she consents, never consent itself.
private struct ExitMargin: View {
    let engine: Engine
    @SwiftUI.State private var days: String
    @SwiftUI.State private var saved = false

    init(engine: Engine) {
        self.engine = engine
        _days = SwiftUI.State(initialValue: String(engine.prefs.exitMarginDays))
    }

    var body: some View {
        let value = Int(typed: days).flatMap { (1...30).contains($0) ? $0 : nil }
        VStack(alignment: .leading, spacing: 0) {
            Say(text: "Time reserved for a forced close to play out (your exit margin), about 14 days by default. Nothing closes on its own unless you allow exits below. Your wallet must stay online and confirm its timeout promptly; a hub may answer late until that timeout wins.")
            HStack(alignment: .center, spacing: 12) {
                Field(value: days, change: { days = $0; saved = false }, label: "Days", hint: "1 to 30, estimated from block times", failed: value == nil, number: true)
                FilledTonalButton(
                    onClick: {
                        if let value { Task { if case .success = await engine.setExitMargin(days: value) { saved = true } } }
                    },
                    enabled: value != nil && !saved
                ) { Text(saved ? "Saved" : "Save") }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

/// Her consent to exit contracts ending, with the address their notes are
/// paid out to: given, shown and revoked here only, never assumed.
private struct ExitConsentEntry: View {
    let engine: Engine
    let contracts: Bool
    @SwiftUI.State private var consent: ExitConsent? = nil
    @SwiftUI.State private var address = ""
    @SwiftUI.State private var error: String? = nil

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Exits").textStyle(Theme.type.bodyLarge)
            Say(
                text: "Allowed, as a contract nears its end and no other at its hub protects your notes there, Licash pays " +
                    "those notes out to your address for at most 1% and 1,000 sat, then closes the contract early. " +
                    "If they are not paid out a day before its last safe block, it forces the contract closed. " +
                    "Not allowed, it only tells you. Revoking stops new steps; steps under way are finished."
            )
            if let given = consent {
                Say(text: "Allowed, paying out to \(given.address).")
                FilledTonalButton(onClick: {
                    Task {
                        switch await engine.revokeExitConsent() {
                        case .success: consent = nil
                        case .failure(let e): error = explain(e: e)
                        }
                    }
                }) { Text("Revoke") }
                    .accessibilityIdentifier("revoke-exits")
            } else if !contracts {
                Say(text: "Asked once a contract protects your notes.")
            } else {
                Field(value: address, change: { address = $0; error = nil }, label: "Bitcoin address", hint: error ?? "Where your notes are paid out to", failed: error != nil)
                FilledTonalButton(
                    onClick: {
                        Task {
                            switch await engine.consentToExit(address: address.trimmingCharacters(in: .whitespacesAndNewlines)) {
                            case .success(let given): consent = given
                            case .failure(let e): error = explain(e: e)
                            }
                        }
                    },
                    enabled: !address.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                ) { Text("Allow exits") }
                    .accessibilityIdentifier("allow-exits")
            }
        }
        .padding(.vertical, 12)
        .task(id: contracts) { consent = try? await engine.exitConsent().get() }
    }
}

/// The light client's nodes and block server, and the block explorer that payments link to.
private struct LightClient: View {
    let engine: Engine
    @SwiftUI.State private var peers: String
    @SwiftUI.State private var cdn: String
    @SwiftUI.State private var explorer: String
    // Applying restarts the light client: said while it is, then that it was.
    @SwiftUI.State private var applying = false
    @SwiftUI.State private var applied = false
    @SwiftUI.State private var applyError: String? = nil
    @SwiftUI.State private var applyWait = Wait()

    init(engine: Engine) {
        self.engine = engine
        _peers = SwiftUI.State(initialValue: engine.prefs.peers)
        _cdn = SwiftUI.State(initialValue: engine.prefs.cdn)
        _explorer = SwiftUI.State(initialValue: engine.prefs.explorer)
    }

    var body: some View {
        let feel = rememberFeel()
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 4) {
                Field(value: peers, change: { peers = $0; applied = false }, label: "Nodes", hint: "host:port, separated by commas")
                Field(value: cdn, change: { cdn = $0; applied = false }, label: "Block server", hint: "default, a URL, or empty for none")
                FilledTonalButton(
                    onClick: {
                        applying = true
                        applied = false
                        applyError = nil
                        Task {
                            switch await engine.setChain(peers: peers, cdn: cdn) {
                            case .success:
                                applied = true
                                feel.confirm()
                            case .failure(let e):
                                applyError = explain(e: e)
                                feel.reject()
                            }
                            applying = false
                        }
                    },
                    enabled: !applying
                ) {
                    HStack(alignment: .center, spacing: 0) {
                        if applying {
                            CircularProgressIndicator(size: 16, strokeWidth: 2)
                            Spacer().frame(width: 8)
                        }
                        Text(applying ? "Applying…" : "Apply")
                    }
                }
                .accessibilityIdentifier("apply-chain")
                Waited(wait: applyWait)
                if applied {
                    Text("Applied")
                        .textStyle(Theme.type.bodySmall)
                        .foregroundStyle(Theme.color.onSurfaceVariant)
                }
                if let applyError { Problem(text: applyError) }
            }
            .padding(.vertical, 12)
            Divider()
            HStack(alignment: .center, spacing: 12) {
                Field(value: explorer, change: { explorer = $0 }, label: "Block explorer", hint: "default, a URL, or empty for none")
                FilledTonalButton(onClick: { engine.prefs.explorer = explorer }) { Text("Save") }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.vertical, 12)
        }
        .rememberWait(wait: applyWait, waiting: applying)
    }
}

/// A setting typed in: its name in the field, and a hint under it, red if `failed`.
private struct Field: View {
    let value: String
    let change: (String) -> Void
    let label: String
    let hint: String
    var failed: Bool = false
    var number: Bool = false

    var body: some View {
        OutlinedTextField(value: value, onValueChange: change, label: label, isError: failed, supportingText: hint, singleLine: true)
            .keyboardType(number ? .numberPad : .URL)
            .textInputAutocapitalization(.never)
            .autocorrectionDisabled()
            .submitLabel(.done)
            .frame(maxWidth: .infinity, alignment: .leading)
    }
}
