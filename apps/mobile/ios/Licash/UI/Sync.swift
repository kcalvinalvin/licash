import Foundation
import SwiftUI

/// Where a step of catching up with Bitcoin stands.
enum Progress { case done, doing, waiting }

/// A step of catching up with Bitcoin: what it is, where it stands, what it
/// is doing in a few words, and how far, 0 to 1, only when what it counts
/// to is known.
struct Stage: Equatable {
    var name: String
    var state: Progress
    var detail: String
    var fraction: Double? = nil

    /// Whether the time it takes is shown: a step that can take many minutes.
    var timed: Bool { state == .doing && (detail == PREPARING || detail.hasPrefix(STORING)) }
}

/// The pinned headers downloaded, checked before they are stored: no progress told.
let PREPARING = "Preparing, this can take a few minutes"

/// The pinned headers checked, stored before the light client starts, how far told.
let STORING = "Storing"

/// How far `done` is of the way from `from` to `to`, if that way is known and `done` is on it.
func fraction(from: Int64, done: Int64, to: Int64) -> Double? {
    // In Float, as shown(f:) says it: 29 of 100 is 29%, where Double's 0.29 * 100 truncates to 28%.
    to > from && from <= done && done <= to ? Double(Float(done - from) / Float(to - from)) : nil
}

// In Float, as the fraction was worked out.
private func shown(f: Double?) -> String { f.map { " (\(Int(Float($0) * 100))%)" } ?? "" }

// A count gone negative wraps rather than traps.
private func block(n: Int64) -> String { digits(amount: UInt64(bitPattern: n)) }

/// The steps of catching up with Bitcoin for `ui`, in their order: Tor if
/// on, the download of the headers this release pins while under way, the
/// nodes, the block headers, the block filters, then the wallet's
/// payments. A step waits for the one before it.
func stages(ui: Ui) -> [Stage] {
    var out: [Stage] = []
    var before = true
    // A step waiting says so, not where it would be (no block 0 of 0).
    func add(name: String, done: Bool, doing: String, finished: String, fraction: Double? = nil, waiting: String = "Waiting") {
        let state: Progress = if !before { .waiting } else if done { .done } else { .doing }
        let detail = switch state {
        case .done: finished
        case .doing: doing
        case .waiting: waiting
        }
        out.append(Stage(name: name, state: state, detail: detail, fraction: state == .doing ? fraction : nil))
        before = before && done
    }
    let tor = ui.torStatus
    if ui.tor {
        let ready = tor?.ready == true
        let doing = if let tor { tor.blocked.map { "Stuck: \($0)" } ?? tor.summary } else { "Starting" }
        add(name: TOR, done: ready, doing: doing, finished: "Connected", fraction: tor.map { Double(Float($0.percent) / 100) })
    }
    let chain = ui.chain
    if chain.importing >= 0 {
        let f = Double(Float(chain.importing) / 100)
        // Downloaded, the client checks them (minutes, uncounted), then stores them.
        let stored = chain.storing >= 0 ? Double(Float(chain.storing) / 100) : nil
        if chain.importing < 100 {
            add(name: HEADERS_DOWNLOAD, done: false, doing: "Downloading" + shown(f: f), finished: "", fraction: f)
        } else if let stored {
            // Downloaded: named for what it does now, not a download.
            add(name: HEADERS_SETUP, done: false, doing: STORING + shown(f: stored), finished: "", fraction: stored)
        } else {
            add(name: HEADERS_SETUP, done: false, doing: PREPARING, finished: "")
        }
    }
    let nodes = "\(chain.peers) " + (chain.peers == 1 ? "node" : "nodes")
    add(name: NODES, done: chain.peers > 0, doing: "Looking for nodes", finished: nodes)
    let target = chain.peerHeight > 0 ? Int64(chain.peerHeight) : nil
    let headers = Int64(chain.headers)
    let headed = chain.synced || (target.map { headers >= $0 } ?? false)
    add(
        name: "Block headers",
        done: headed,
        doing: "Block \(block(n: headers))" + (target.map { " of \(block(n: $0))" } ?? "") +
            shown(f: target.flatMap { fraction(from: 0, done: headers, to: $0) }),
        finished: "Block \(block(n: headers))",
        fraction: target.flatMap { fraction(from: 0, done: headers, to: $0) }
    )
    let filters = Int64(chain.height)
    add(
        name: "Block filters",
        done: chain.synced,
        doing: "Block \(block(n: filters)) of \(block(n: headers))" + shown(f: fraction(from: 0, done: filters, to: headers)),
        finished: "Block \(block(n: filters))",
        fraction: fraction(from: 0, done: filters, to: headers)
    )
    if ui.hubs.isEmpty {
        add(name: PAYMENTS, done: false, doing: "Choose a hub first", finished: "", waiting: "Choose a hub first")
        return out
    }
    guard let scan = ui.scan else {
        add(name: PAYMENTS, done: false, doing: "Starting", finished: "")
        return out
    }
    let next = Int64(scan.next)
    let reachable = Int64(scan.height)
    var from = Int64(scan.from)
    if !(1 <= from && from <= next) { from = next }
    // Looked in up to the block the light client had then: a block come
    // since is looked in at the wallet's next round.
    let newer = filters > reachable
    let looked = next > reachable
    let doing = looked && newer
        ? "Checked to block \(block(n: reachable)), \(NEXT_BLOCK) \(block(n: reachable + 1))"
        : "Block \(block(n: next)) of \(block(n: reachable)), \(block(n: reachable - next + 1)) to go" +
            shown(f: fraction(from: from, done: next, to: reachable + 1))
    let finished = "Up to date" + (scan.tweaks ? "" : ", fallback addresses only")
    add(name: PAYMENTS, done: looked && !newer, doing: doing, finished: finished, fraction: looked ? nil : fraction(from: from, done: next, to: reachable + 1))
    return out
}

/// Whether a step is under way, the wallet catching up.
func catchingUp(ui: Ui) -> Bool { stages(ui: ui).contains { $0.state != .done } }

/// Whether Home shows each step of catching up: a new wallet's first only, later ones told by the connections dot.
func firstCatchingUp(ui: Ui) -> Bool { !ui.caughtUp && ui.history.isEmpty && catchingUp(ui: ui) }

let TOR = "Tor"
let HEADERS_DOWNLOAD = "Block headers download"
let HEADERS_SETUP = "Block headers setup"
let NODES = "Bitcoin nodes"
let PAYMENTS = "Your payments"
private let NEXT_BLOCK = "checking block"

/// How long Bitcoin may be behind, once caught up, before its dot says so:
/// a block arriving is looked in within a round or two, as usual, not a wait.
let STALE_MS: Int64 = 30_000

/// A step under way in a few plain words: in advanced mode, its name and
/// where it is, block numbers and all.
func said(stage: Stage, developer: Bool) -> String {
    if developer { return "\(stage.name): \(stage.detail)" }
    if stage.state == .waiting { return "Waiting for Tor" }
    if stage.name == TOR { return stage.detail }
    if stage.name == HEADERS_DOWNLOAD || stage.name == HEADERS_SETUP {
        if stage.detail == PREPARING { return PREPARING }
        if stage.detail.hasPrefix(STORING) { return "Storing block headers" + shown(f: stage.fraction) }
        return "Downloading block headers" + shown(f: stage.fraction)
    }
    if stage.name == NODES { return "Looking for Bitcoin nodes" }
    if stage.name == PAYMENTS && stage.detail.contains(NEXT_BLOCK) { return "Checking the newest block" }
    if stage.name == PAYMENTS { return "Looking for your payments" + shown(f: stage.fraction) }
    return "Catching up with Bitcoin" + shown(f: stage.fraction)
}

/// How a connection stands, from fine to failing.
enum Reach: Comparable { case up, going, down }

/// A connection the app needs, how it stands in a few words, and how long it last took to answer, if known.
struct Link: Equatable {
    var name: String
    var reach: Reach
    var detail: String
    var ms: Int64? = nil
}

/// The app's connections: Tor if on, then Bitcoin, up once caught up (a
/// wallet with no hub yet does not look for payments, and is not behind).
/// Once the wallet has caught up, Bitcoin stays up while it keeps up with
/// new blocks as usual: behind only once `stale`, [STALE_MS] behind.
func links(ui: Ui, stale: Bool = true) -> [Link] {
    var out: [Link] = []
    let tor = ui.torStatus
    if ui.tor {
        if tor?.ready == true {
            out.append(Link(name: TOR, reach: .up, detail: "Connected"))
        } else if let blocked = tor?.blocked {
            out.append(Link(name: TOR, reach: .down, detail: "Stuck: \(blocked)"))
        } else {
            out.append(Link(name: TOR, reach: .going, detail: "Connecting" + (tor.map { " (\($0.percent)%)" } ?? "")))
        }
    }
    let step = bitcoinStep(ui: ui)
    let chain = ui.chain
    let up = "Up to date" + (ui.developer ? ", block \(block(n: Int64(chain.height))), \(chain.peers) nodes" : "")
    let ping = chain.pingMs > 0 && chain.peers > 0 ? Int64(chain.pingMs) : nil
    let keepingUp = step != nil && ui.caughtUp && !stale
    if step == nil || keepingUp {
        out.append(Link(name: BITCOIN, reach: .up, detail: up, ms: ping))
    } else if let step {
        out.append(Link(name: BITCOIN, reach: .going, detail: said(stage: step, developer: ui.developer), ms: ping))
    }
    for hub in ui.hubs {
        switch ui.hubsReached[hub.url] {
        case true?: out.append(Link(name: name(url: hub.url), reach: .up, detail: "Reached", ms: ui.hubsMs[hub.url]))
        case false?: out.append(Link(name: name(url: hub.url), reach: .down, detail: "Not answering"))
        case nil: out.append(Link(name: name(url: hub.url), reach: .going, detail: "Not asked yet"))
        }
    }
    // Only a wallet with a mailbox looks in one.
    if ui.hubs.contains(where: { $0.mailbox }) {
        switch ui.relaysReached {
        case true?:
            switch ui.relaysFull {
            case false: out.append(Link(name: RELAYS, reach: .up, detail: "Reached", ms: ui.relaysMs))
            case true: out.append(Link(name: RELAYS, reach: .down, detail: "Full of mail: newer mail may hide a payment to you", ms: ui.relaysMs))
            }
        case false?: out.append(Link(name: RELAYS, reach: .down, detail: "None reached: payments to you wait there until one is"))
        case nil: out.append(Link(name: RELAYS, reach: .going, detail: "Not looked in yet"))
        }
    }
    return out
}

/// The step of catching up with Bitcoin under way, if any: not Tor's, nor looking for payments with no hub.
func bitcoinStep(ui: Ui) -> Stage? {
    stages(ui: ui).first { $0.state != .done && $0.name != TOR && !($0.name == PAYMENTS && ui.hubs.isEmpty) }
}

/// Whether Bitcoin is behind now, as its connection says once [STALE_MS] long.
func bitcoinBehind(ui: Ui) -> Bool { bitcoinStep(ui: ui) != nil }

/// Bitcoin, as a connection is named.
let BITCOIN = "Bitcoin"

/// The mailbox's relays, as a connection is named.
let RELAYS = "Mailbox relays"

/// How long Tor may not get through before turning it off is offered: a first start takes a minute or two.
let TOR_PATIENCE_MS: Int64 = 180_000

/// Whether Tor keeps the app from Bitcoin now: on, and not connected, or no
/// node reached through it while the light client looks for some (not while
/// it imports the headers this release pins, minutes with no node asked).
func torInTheWay(ui: Ui) -> Bool {
    ui.tor && (ui.torStatus?.ready != true || (ui.chain.peers == 0 && ui.chain.importing < 0))
}

/// Whether turning Tor off is offered, it in the way for `stuckMs` (nil if
/// not): at once if Tor says it is stuck, else after [TOR_PATIENCE_MS].
func offerTorOff(ui: Ui, stuckMs: Int64?) -> Bool {
    torInTheWay(ui: ui) && (ui.torStatus?.blocked != nil || (stuckMs ?? 0) >= TOR_PATIENCE_MS)
}

/// What turning Tor off means, said beside the button: what is wrong, Tor connected or not.
func torOffMeans(ui: Ui) -> String {
    (ui.torStatus?.ready == true ? "No Bitcoin node answers through Tor. " : "Tor is not getting through. ") +
        "Without it the app connects directly: faster, but hubs, relays and Bitcoin nodes see your IP address. " +
        "You can turn it on again in Settings."
}

/// The app's state kept fresh every two seconds while shown.
private struct KeptFresh: View {
    let engine: Engine

    init(engine: Engine) {
        self.engine = engine
    }

    var body: some View {
        Color.clear
            .frame(width: 0, height: 0)
            .task {
                while true {
                    await engine.live()
                    do { try await Task.sleep(for: .seconds(2)) } catch { return }
                }
            }
    }
}

/// The steps of catching up, each with its progress, kept fresh every two
/// seconds while shown.
struct SyncStages: View {
    let engine: Engine
    let ui: Ui

    init(engine: Engine, ui: Ui) {
        self.engine = engine
        self.ui = ui
    }

    @State private var preparing = Wait()

    var body: some View {
        KeptFresh(engine: engine)
        VStack(alignment: .leading, spacing: 0) {
            ForEach(stages(ui: ui), id: \.name) { stage in StageRow(stage: stage, preparing: preparing) }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("sync-stages")
        .rememberWait(wait: preparing, waiting: ui.chain.importing >= 100)
    }
}

/// Catching up, on Home: each step in advanced mode, else only the one
/// under way, in plain words, and how far.
struct CatchingUp: View {
    let engine: Engine
    let ui: Ui

    init(engine: Engine, ui: Ui) {
        self.engine = engine
        self.ui = ui
    }

    @State private var preparing = Wait()

    var body: some View {
        if ui.developer {
            SyncStages(engine: engine, ui: ui)
        } else {
            KeptFresh(engine: engine)
                .rememberWait(wait: preparing, waiting: ui.chain.importing >= 100)
            if let step = stages(ui: ui).first(where: { $0.state != .done }) {
                VStack(alignment: .leading, spacing: 0) {
                    Text(said(stage: step, developer: false)).textStyle(Theme.type.bodyLarge)
                    if step.timed { Waited(wait: preparing) }
                    Spacer().frame(height: 6)
                    if let f = step.fraction {
                        LinearProgressIndicator(progress: f).frame(maxWidth: .infinity)
                    } else {
                        LinearProgressIndicator().frame(maxWidth: .infinity)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.vertical, 8)
                .accessibilityElement(children: .contain)
                .accessibilityIdentifier("catching-up")
            }
        }
    }
}

/// A dot, by the network, for how the app's connections stand: a tap
/// shows each, and offers turning Tor off once it keeps the app from
/// Bitcoin.
struct Connections: View {
    let engine: Engine
    let ui: Ui

    init(engine: Engine, ui: Ui) {
        self.engine = engine
        self.ui = ui
    }

    // Bitcoin behind since when: said once it lasts STALE_MS.
    @State private var behind = Wait()
    @State private var stale = false
    // Counted while closed too: how long Tor has been in the way.
    @State private var stuck = Wait()
    @State private var open = false
    @State private var error: String? = nil
    @State private var preparing = Wait()

    var body: some View {
        let links = links(ui: ui, stale: stale)
        let worst = links.map(\.reach).max()!
        let described = switch worst {
        case .up: "Connections: up to date"
        case .going: "Connections: connecting"
        case .down: "Connections: a problem"
        }
        Button {
            // What the sheet remembers starts afresh each time it opens.
            preparing = Wait()
            error = nil
            open = true
        } label: {
            Circle()
                .fill(Licash.tint(reach: worst))
                .frame(width: 10, height: 10)
                .frame(width: 32, height: 32)
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(described)
        .accessibilityIdentifier("connections")
        .rememberWait(wait: behind, waiting: bitcoinBehind(ui: ui))
        .rememberWait(wait: stuck, waiting: torInTheWay(ui: ui))
        .task(id: behind.since) {
            stale = false
            guard let since = behind.since else { return }
            let ms = max(since + STALE_MS - Int64(clock_gettime_nsec_np(CLOCK_MONOTONIC) / 1_000_000), 0)
            do { try await Task.sleep(for: .milliseconds(ms)) } catch { return }
            stale = true
        }
        .sheet(isPresented: $open) {
            Sheet(title: "Connections", dismiss: { open = false }) {
                KeptFresh(engine: engine)
                ForEach(Array(links.enumerated()), id: \.offset) { _, link in
                    // In advanced mode, Bitcoin's steps while catching up, under it, not said twice.
                    let steps = link.name == BITCOIN && ui.developer && link.reach != .up
                    LinkRow(link: steps ? { var c = link; c.detail = "Catching up"; return c }() : link)
                    if steps {
                        VStack(alignment: .leading, spacing: 0) {
                            ForEach(stages(ui: ui).filter { $0.name != TOR }, id: \.name) { stage in
                                StageRow(stage: stage, preparing: preparing)
                            }
                        }
                        .padding(.leading, 22)
                        .accessibilityElement(children: .contain)
                        .accessibilityIdentifier("bitcoin-steps")
                    }
                }
                let since = stuck.since
                if offerTorOff(ui: ui, stuckMs: since.map { Int64(clock_gettime_nsec_np(CLOCK_MONOTONIC) / 1_000_000) - $0 }) {
                    Spacer().frame(height: 8)
                    Say(text: torOffMeans(ui: ui))
                    OutlinedButton(onClick: {
                        Task { if case .failure(let e) = await engine.setTor(on: false) { error = explain(e: e) } }
                    }) { Text("Turn Tor off").frame(maxWidth: .infinity) }
                    .accessibilityIdentifier("tor-off")
                }
                if let error { Problem(text: error) }
            }
            .rememberWait(wait: preparing, waiting: ui.chain.importing >= 100)
        }
    }
}

private func tint(reach: Reach) -> Color {
    switch reach {
    case .up: return Theme.color.primary
    case .going: return Theme.color.outline
    case .down: return Theme.color.error
    }
}

private struct LinkRow: View {
    let link: Link

    init(link: Link) {
        self.link = link
    }

    var body: some View {
        HStack(alignment: .center, spacing: 0) {
            Circle().fill(Licash.tint(reach: link.reach)).frame(width: 10, height: 10)
            Spacer().frame(width: 12)
            VStack(alignment: .leading, spacing: 0) {
                Text(link.name).textStyle(Theme.type.bodyLarge)
                Text(link.detail)
                    .accessibilityIdentifier("link \(link.name)")
                    .textStyle(Theme.type.bodySmall)
                    .foregroundStyle(Theme.color.onSurfaceVariant)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            if let ms = link.ms {
                Text(latency(ms: ms))
                    .padding(.leading, 8)
                    .accessibilityIdentifier("ms \(link.name)")
                    .textStyle(Theme.type.bodySmall)
                    .foregroundStyle(Theme.color.onSurfaceVariant)
            }
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 8)
    }
}

/// A time to answer, as shown beside a connection: `85 ms`, `1,240 ms`.
func latency(ms: Int64) -> String { "\(digits(amount: UInt64(bitPattern: ms))) ms" }

private struct StageRow: View {
    let stage: Stage
    let preparing: Wait

    init(stage: Stage, preparing: Wait) {
        self.stage = stage
        self.preparing = preparing
    }

    var body: some View {
        HStack(alignment: .top, spacing: 0) {
            switch stage.state {
            case .done:
                Image(systemName: "checkmark.circle.fill")
                    .resizable()
                    .scaledToFit()
                    .frame(width: 20, height: 20)
                    .foregroundStyle(Theme.color.primary)
                    .accessibilityHidden(true)
            case .doing:
                CircularProgressIndicator(size: 20, strokeWidth: 2)
            case .waiting:
                Circle().strokeBorder(Theme.color.outline, lineWidth: 2).frame(width: 16, height: 16).padding(2)
            }
            Spacer().frame(width: 12)
            VStack(alignment: .leading, spacing: 0) {
                Text(stage.name).textStyle(Theme.type.bodyLarge)
                if !stage.detail.isEmpty {
                    Text(stage.detail)
                        .accessibilityIdentifier("stage \(stage.name)")
                        .textStyle(Theme.type.bodySmall)
                        .foregroundStyle(Theme.color.onSurfaceVariant)
                }
                if stage.timed { Waited(wait: preparing) }
                if let f = stage.fraction {
                    Spacer().frame(height: 6)
                    LinearProgressIndicator(progress: f).frame(maxWidth: .infinity)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 8)
    }
}
