import Chain
import Foundation
import LicashCore
import Observation

/// What the screens show.
struct Ui: Equatable {
    var loaded: Bool = false
    var wallet: Bool = false
    var network: Network = .bitcoin
    var developer: Bool = false
    var balance: LicashCore.Balance = LicashCore.Balance(
        notes: 0, notesPending: 0, onchain: 0, moving: 0, kept: 0, opening: 0, covered: 0
    )
    var history: [Activity] = []
    var hubs: [Hub] = []
    /// The hubs charging more than agreed as a contract opened there: not paid until approved.
    var raised: [FeesRaised] = []
    /// Note work waiting for another sync, including privacy rest.
    var noteMaintenance: Bool = false
    var tor: Bool = false
    var chain: ChainStatus = ChainStatus()
    /// How far looking for payments on Bitcoin got, once looking.
    var scan: Scan? = nil
    /// Whether the wallet's first catching up is over: from then on, the connections dot tells of it.
    var caughtUp: Bool = false
    /// How far the app's Tor client is in connecting, over Tor.
    var torStatus: TorStatus? = nil
    /// Payments on Bitcoin on their way: in no block yet.
    var incoming: [Activity] = []
    /// Why the last round of syncing fell short, if it did.
    var problem: String? = nil
    /// Why the wallet on this device could not be opened, if it could not: none made over it.
    var unopened: String? = nil
    /// Whether each hub answered when last asked, by URL: none before it is asked. In memory only.
    var hubsReached: [String: Bool] = [:]
    /// How fast each hub reached answers, in milliseconds, by URL: its fastest answer of the last minutes.
    var hubsMs: [String: Int64] = [:]
    /// Whether the mailbox's relays were reached when last looked in, if looked in. In memory only.
    var relaysReached: Bool? = nil
    /// Whether a relay's answer was all a look in the mailbox asks for when
    /// last looked in: newer mail, junk perhaps, may hide an older payment
    /// there. In memory only.
    var relaysFull: Bool = false
    /// How long looking in the mailbox's relays took when they answered, in milliseconds.
    var relaysMs: Int64? = nil
    /// The recovery contracts with hubs, the newest first.
    var contracts: [Contract] = []
    /// The contracts forced closed, the oldest first.
    var recoveries: [Recovery] = []
    /// Each live contract's exit, as the last round left it.
    var exits: [ContractExit] = []
    /// The hub a contract is being opened at, while one is.
    var opening: String? = nil
    /// The contract offered for what was paid on Bitcoin, if any: opened only
    /// once its owner accepts it, and not offered unasked once `offerDeclined`.
    var offer: ContractQuote? = nil
    var offerDeclined: Bool = false
    /// Where each line of the history under way stands, by [key].
    var under: [String: Details] = [:]
    /// The contact each line of the history was paid to or by, by [key].
    var contacts: [String: String] = [:]
    /// How closely what is under way is watched, out of sight too.
    var watch: Watch = Watch.none
    /// When a Lightning invoice was last asked, in seconds since the epoch.
    var asked: Int64? = nil
    /// Payments made still on their way to their payees: kept at their hubs, or to mail.
    var onTheirWay: UInt32 = 0
    /// Cold storage, if configured: notes withdrawn to an external wallet.
    var cold: ColdStorage? = nil
    /// Automatic backups, if a folder is chosen.
    var backup: AutoBackup? = nil
}

extension Activity {
    /// A line of the history's own key: lines of different kinds can share an id.
    var key: String { "\(kind) \(id)" }
}

/// Ordinary progress details are bounded, payout deadlines are always read.
private let UNDER_READ = 20

func detailsToRead(history: [Activity]) -> [Activity] {
    history
        .filter { $0.state == .pending }
        .enumerated()
        .filter { index, activity in index < UNDER_READ || activity.kind == .payout }
        .map { $0.element }
}

/// How the light client stands: the height of its block filters (`height`,
/// what the wallet looks in), of its block headers, and of the best of its
/// peers, which it catches up to (0 if none told).
struct ChainStatus: Equatable {
    var height: Int = 0
    var peers: Int64 = 0
    var synced: Bool = false
    var headers: Int = 0
    var peerHeight: Int = 0
    /// How far, in percent, the headers this release pins are downloaded, before the client starts: -1 if not.
    var importing: Int = -1
    /// The shortest last ping of a connected peer, in milliseconds: -1 if none answered one.
    var pingMs: Int = -1
    /// How far, in percent, the headers downloaded are stored, before the client starts: -1 if not, or not told yet.
    var storing: Int = -1
}

/// How `light` stands now.
private func statusOf(light: MobileChain) throws -> ChainStatus {
    var height: Int32 = 0
    try light.height(&height)
    var headers: Int32 = 0
    do {
        try light.headers(&headers)
    } catch {
        headers = 0
    }
    return ChainStatus(
        height: Int(height),
        peers: Int64(light.peers()),
        synced: light.synced(),
        headers: Int(headers),
        peerHeight: Int(light.peerHeight()),
        importing: Int(light.importing()),
        pingMs: Int(light.pingMs()),
        storing: Int(light.storing())
    )
}

/// Automatic backups to a folder: when last written whole, and what failed since, if anything.
struct AutoBackup: Equatable {
    var at: Int64?
    var problem: String? = nil
}

/// Something to tell the user once, `received` if it is money in.
struct Event: Equatable {
    var text: String
    var received: UInt64? = nil
}

/// A contract's terms its owner chose, within its hub's range: up to `cap`
/// sat over `blocks`, paid from `coin` (`txid:vout`), `funded` sat of it
/// funded by her from that coin, issued back to her balance once its
/// funding confirms, the rest of the coin back on Bitcoin.
struct Chosen: Equatable {
    var cap: UInt64
    var blocks: UInt32
    var coin: String
}

/// The wallet and the light client, and the loop keeping them in step while
/// the app is in sight, or out of sight while a payment is under way (see
/// [Watch]): looking for payments on Bitcoin, offering to protect them,
/// finishing what is under way and taking what arrives in the mailbox. Every wallet call is off the main thread, those
/// reaching the network or changing what the wallet holds one at a time,
/// those reading it alongside.
@MainActor @Observable
final class Engine {
    let prefs: Prefs
    let notifier: Notifier

    private let files: URL
    private let cacheDir: URL
    private let walletDir: String
    private let mutex = Mutex()
    private let lightLock = Mutex()
    private(set) var ui: Ui
    /// Those told of each event, a stream each ([events]).
    private var notices: [UUID: AsyncStream<Event>.Continuation] = [:]
    /// A poke not waited for yet, kept for the next wait ([wake]).
    private var woken = false
    /// The loop's wait for a poke while it waits, and which wait it is.
    private var waking: CheckedContinuation<Void, Never>?
    private var wakes = 0

    /// Look often: someone is waiting to be paid. In the mailbox too, apart
    /// from the rounds, every [MAIL_MS]: its relays kept connected and
    /// pushing the new mail, a look takes what they pushed at once, asking
    /// them for all of it every 30 s (a few seconds over Tor).
    var eager = false {
        didSet {
            if eager { watchMailbox() }
        }
    }

    /// Looking in the mailbox apart from the rounds.
    private var watchingMailbox = false

    /// Waiting, in sight, for the mailbox's relays to push mail ([awaitMail]).
    private var awaitingMail = false

    private var foreground = false

    /// Those keeping the engine at work out of sight: the service, the periodic job.
    private var keepers = 0

    /// The rounds done in step with Bitcoin, its light client synced.
    private var rounds: Int64 = 0

    /// Whether the periodic look is scheduled, as last asked: unknown at start.
    private var looking: Bool?

    /// The events told of out of sight, each in a notification of its own.
    private var noticed = 0

    /// Every line of the history when last looked, since the app started.
    private var seen: Set<String>?

    /// Whether the wallet was opened and its light client first started, or not.
    private var startedUp = false

    /// Closed as it is dropped: the core frees it once nothing holds it.
    private var wallet: Wallet?

    private var light: MobileChain?
    private var lightOf: String?

    /// When the light client last started ([elapsedRealtime]),
    /// and the steps of catching up logged since, each with the time it took.
    private var lightAt: Int64 = 0
    private var phases: Set<String> = []

    /// The app's Tor client, made once first needed over Tor ([tor]).
    private var torClient: TorClient?

    /// Making the app's Tor client: those asking meanwhile wait for the one made.
    private let torLock = Mutex()

    /// The coins on Bitcoin and payments on their way already told of.
    private var told: Set<String>?

    /// What was on Bitcoin when a contract was last looked for to offer, and when, in ms.
    private var offeredFor: UInt64?
    private var offeredAt: Int64 = 0

    /// How the light client stood when last logged.
    private var logged: ChainStatus?

    /// Whether what the hubs trusted serve was learned since the app started.
    private var hubsLearned = false

    /// When they were last learned, in ms: again every [HUBS_MS], their fees and terms fresh.
    private var hubsLearnedAt: Int64 = 0

    /// Each hub's contract quote, asked ahead so protecting shows at once,
    /// with what was on Bitcoin when asked and when, in ms: fresh for
    /// [QUOTE_MS] while that holds ([contractQuote]).
    private var quotes: [String: (ContractQuote, UInt64, Int64)] = [:]

    /// Asking the hubs for quotes ahead, apart from the rounds.
    private var quoting = false

    /// Carrying on with contracts, apart from the rounds: it may take minutes.
    private var contracting = false

    /// Looking for the swap providers announced, apart from the rounds.
    private var findingProviders = false

    /// The wallet that last looked for swap providers, and when it looks again ([nanoTime]).
    private var providersDue: (Wallet, Int64)?

    /// The wallet whose swap providers were last asked ahead, and when they are again ([nanoTime]).
    private var providersWarmed: (Wallet, Int64)?

    init() {
        let prefs = Prefs()
        self.prefs = prefs
        notifier = Notifier()
        var support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        // Out of the phone's backups: a restored old copy of a wallet would
        // reuse spent state.
        try? FileManager.default.createDirectory(at: support, withIntermediateDirectories: true)
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        try? support.setResourceValues(values)
        files = support
        cacheDir = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
        walletDir = support.appendingPathComponent("wallet").path
        ui = Ui(network: prefs.network, developer: prefs.developer, caughtUp: prefs.caughtUp)
        AppLog.start(dir: files.appendingPathComponent("logs"))
        // A debug build logs what each request over Tor and each relay took.
        #if DEBUG
        let debug = ",licash_tor=debug,licash_nostr=debug,licash_mobile=debug"
        #else
        let debug = ""
        #endif
        setLogger(logger: AppLog.instance, filter: "info,nostr_sdk=warn,nostr_relay_pool=warn\(debug)")
        // A backup found is shown until the app ends, never after: its
        // copied folder of a run before is of no use.
        let copy = restoreCopy
        Task { await off { try? FileManager.default.removeItem(at: copy) } }
        let stream = events()
        Task {
            // In sight, told on screen. Out of sight, money in in a
            // notification: the rest is told as its line finishes.
            for await event in stream {
                if !foreground && event.received != nil {
                    noticed += 1
                    notifier.payment(key: "event \(noticed)", text: event.text)
                }
            }
        }
        Task {
            await load()
            await loop()
        }
        Task {
            // A protection being set up, in sight: its step, how long it
            // has taken and its speed kept current between rounds.
            while true {
                try? await Task.sleep(for: .milliseconds(Engine.OPENING_TICK_MS))
                if foreground && ui.contracts.contains(where: { $0.state == .opening }) { await tickContracts() }
            }
        }
    }

    /// The app in sight: `MainView` tells it as its scene becomes active.
    func started() {
        foreground = true
        // In sight: the only time the service may start.
        if ui.watch == .now { WatchService.start() }
        poke()
        if eager { watchMailbox() }
    }

    /// The app out of sight: `MainView` tells it as its scene goes to the
    /// background. The backup it starts is returned, for iOS to be asked to
    /// wait for it.
    func stopped() -> Task<Void, Never> {
        foreground = false
        // Out of sight the app may stop soon: a change not backed up is now.
        return Task { await autoBackup() }
    }

    /// Something to tell the user once, as it comes: a stream of its own for
    /// each caller, every caller told every event.
    func events() -> AsyncStream<Event> {
        let (stream, notice) = AsyncStream.makeStream(of: Event.self, bufferingPolicy: .bufferingOldest(16))
        let id = UUID()
        notices[id] = notice
        notice.onTermination = { [weak self] _ in
            Task { @MainActor in self?.notices[id] = nil }
        }
        return stream
    }

    /// Tell every caller of [events] of `event`.
    private func emit(event: Event) {
        for notice in notices.values { notice.yield(event) }
    }

    /// `ui` as `change` makes it: told to those watching only if it differs.
    private func update(change: (inout Ui) -> Void) {
        var next = ui
        change(&next)
        if next != ui { ui = next }
    }

    /// The contracts alone read again: cheap enough every few seconds.
    private func tickContracts() async {
        guard let w = wallet, let contracts = try? await off(call: { try w.contracts() }) else { return }
        update { $0.contracts = contracts }
    }

    /// Keep at work out of sight, or no longer: for the service watching.
    func keep(on: Bool) {
        if on { keepers += 1 } else { keepers -= 1 }
        poke()
    }

    /// Keep at work out of sight for two rounds in step with Bitcoin, at most
    /// `ms`: a look for the periodic job, the app perhaps just started. None
    /// with no wallet, or no light client (Tor or the wallet failing).
    func round(ms: Int64) async {
        keepers += 1
        defer { keepers -= 1 }
        let end = ContinuousClock.now + .milliseconds(ms)
        guard await holds(by: end, done: { startedUp }) else { return }
        if wallet == nil || light == nil {
            AppLog.info(line: "no look out of sight: no wallet or no light client")
            return
        }
        let start = rounds
        poke()
        _ = await holds(by: end) { rounds >= start + 2 }
    }

    /// Whether `done` holds before `end`, looked at every tenth of a second:
    /// not once the wait is cancelled.
    private func holds(by end: ContinuousClock.Instant, done: () -> Bool) async -> Bool {
        while !done() {
            if ContinuousClock.now >= end { return false }
            do {
                try await Task.sleep(for: .milliseconds(100))
            } catch {
                return false
            }
        }
        return true
    }

    /// Sync now rather than at the next round.
    func poke() {
        if let waking {
            self.waking = nil
            waking.resume()
        } else {
            woken = true
        }
    }

    /// Wait for a poke, `ms` at most: one since the last wait ends it at once.
    private func wake(ms: Int64) async {
        if woken {
            woken = false
            return
        }
        wakes += 1
        let wait = wakes
        await withCheckedContinuation { done in
            waking = done
            Task {
                try? await Task.sleep(for: .milliseconds(ms))
                if wakes == wait, let waking {
                    self.waking = nil
                    waking.resume()
                }
            }
        }
    }

    /// Look for payments now, as the owner asks by pulling down: those the
    /// hubs keep and the mailbox's at once, as a round does. The chain and
    /// the rest on a round started now.
    func lookNow() async {
        poke()
        guard wallet != nil, let hubs = try? await local(call: { try $0.hubs() }) else { return }
        if !hubs.contains(where: { $0.mailbox }) { return }
        do {
            if try await collect() { await refresh() }
        } catch {
            AppLog.warn(line: "the mailbox, looked in as asked", e: error)
        }
    }

    /// Open the wallet and show it, then start the light client.
    private func load() async {
        await mutex.withLock {
            if walletExists(dir: walletDir) {
                AppLog.info(
                    line: (prefs.tor ? "through Tor" : "Tor off") +
                        (prefs.torChosen ? ", as chosen" : ", by default")
                )
                do {
                    let tor = try await self.tor()
                    let dir = walletDir
                    let opened = try await off { try Wallet.open(dir: dir, tor: tor) }
                    wallet = await configured(w: opened)
                } catch {
                    AppLog.warn(line: "opening the wallet", e: error)
                    update {
                        $0.problem = explain(e: error)
                        $0.unopened = explain(e: error)
                    }
                }
            }
        }
        await refresh()
        update { $0.loaded = true }
        await lightLock.withLock { await startLight() }
        startedUp = true
    }

    /// The app's Tor client, over Tor: made the first time, its state moved
    /// out of the wallet's directory where an earlier version kept it. None
    /// with Tor off. It lives as long as the app, wallet or not.
    private func tor() async throws -> TorClient? {
        if !prefs.tor { return nil }
        if let torClient { return torClient }
        return try await torLock.withLock {
            if let torClient { return torClient }
            let dir = files.appendingPathComponent("tor").path
            let legacy = URL(fileURLWithPath: walletDir).appendingPathComponent("tor").path
            let made = try await off { try TorClient(dir: dir, legacy: legacy) }
            torClient = made
            AppLog.info(line: "Tor's state kept in \(made.dir())")
            return made
        }
    }

    /// The app's Tor client closed, once no wallet reaches anything through it.
    private func closeTor() async {
        // The core frees it once nothing holds it.
        await torLock.withLock { torClient = nil }
    }

    /// Run the light client of the wallet's network, or the one chosen, wallet
    /// or not. Over Tor, everything it reaches goes through the app's Tor
    /// client: never directly instead.
    private func startLight() async {
        var network = prefs.network
        if let w = wallet { network = await off { w.network() } }
        let tor = prefs.tor
        // It holds a password, never logged.
        var proxy: String? = ""
        if tor {
            do {
                let client = try await self.tor()
                proxy = try await off { try client?.proxy() }
            } catch {
                AppLog.warn(line: "the light client's way through Tor", e: error)
                update { $0.problem = explain(e: error) }
                proxy = nil
            }
        }
        let key = [chainName(network: network), prefs.peers, prefs.cdn, proxy ?? "none"].joined(separator: "|")
        if key == lightOf && light != nil { return }
        // Between wallet calls: one may be reading through it.
        await mutex.withLock { await stopLight() }
        guard let proxy else {
            AppLog.info(line: "the light client cannot reach Bitcoin: no way through Tor")
            return
        }
        let name = chainName(network: network)
        let dir = files.appendingPathComponent("chain-\(name)").path
        AppLog.info(line: "starting the light client of \(name)" + (tor ? ", through Tor" : ""))
        let (peers, cdn) = (prefs.peers, prefs.cdn)
        do {
            let made = try await off {
                var error: NSError?
                let made = MobileStartThrough(name, dir, peers, cdn, proxy, &error)
                guard let made else { throw error ?? ChainError.Failed(reason: "the light client failed") }
                return made
            }
            light = made
            lightOf = key
            lightAt = Engine.elapsedRealtime()
            phases.removeAll()
            AppLog.info(line: "light client of \(name) started")
        } catch {
            AppLog.warn(line: "starting the light client", e: error)
            update { $0.problem = explain(e: error) }
        }
    }

    /// What to tell of a step that needs the wallet's coins on Bitcoin known
    /// while the app is behind it, with how far it is: none once caught up.
    private func stillCatchingUp() -> String? {
        let ui = self.ui
        if !behind(ui: ui) { return nil }
        guard let step = stages(ui: ui).first(where: { $0.state != Progress.done }) else { return nil }
        return "Still catching up with Bitcoin (\(step.name.lowercased()): \(step.detail)): " +
            "try again once caught up."
    }

    /// Since when, in sight, the app is behind Bitcoin ([elapsedRealtime]).
    private var behindSince: Int64?

    /// Behind Bitcoin in sight for a while, as on a first start or after
    /// days away: the service keeps catching up should she leave the app,
    /// its progress in its notification. A catch-up of seconds shows none.
    private func keepCatchingUp() {
        if !behind(ui: ui) {
            behindSince = nil
            return
        }
        let now = Engine.elapsedRealtime()
        let since = behindSince ?? now
        behindSince = since
        if now - since >= Engine.CATCH_UP_AFTER_MS { WatchService.start() }
    }

    // Automatic backups. Each field below is read and written under
    // [backupLock] only.

    /// The wallet's files as last seen ([Wallet.backupStamp]), and as they were when last backed up.
    private var seenStamp: String?
    private var backedStamp: String?

    /// Since when, and when last, the wallet changed with no backup since ([elapsedRealtime]).
    private var changedFirst: Int64 = 0
    private var changedLast: Int64 = 0

    /// When a backup was last looked at, and none tried again before when, after a failure.
    private var backupLooked: Int64 = 0
    private var backupRetry: Int64 = 0
    private let backupLock = Mutex()

    /// An automatic backup, if a folder is chosen: the wallet changed, once
    /// it rested for 30 seconds, or 5 minutes after a change at the latest,
    /// at once out of sight where the app may stop soon. Else hourly, which
    /// writes one only if the last is a day old or gone from the folder.
    /// Rust tells whether a change is worth one.
    private func autoBackup() async {
        guard let tree = prefs.backupFolder, let w = wallet else { return }
        // One at a time: another under way sees this change too.
        if !backupLock.tryLock() { return }
        defer { backupLock.unlock() }
        let now = Engine.elapsedRealtime()
        let stamp = await off { w.backupStamp() }
        if stamp != seenStamp {
            if seenStamp == backedStamp { changedFirst = now }
            seenStamp = stamp
            changedLast = now
        }
        let due =
            if now < backupRetry {
                false
            } else if stamp != backedStamp {
                !foreground || now - changedLast >= Engine.BACKUP_REST_MS || now - changedFirst >= Engine.BACKUP_MOST_MS
            } else {
                now - backupLooked >= Engine.BACKUP_LOOK_MS
            }
        if due { _ = await backupLocked(tree: tree, w: w, force: false) }
    }

    /// [backupLocked] under [backupLock], off the main thread.
    private func backupNow(tree: String, force: Bool) async -> Result<Void, Error> {
        await backupLock.withLock {
            guard let w = wallet else { return .failure(LicashError.Invalid(reason: "there is no wallet yet")) }
            return await backupLocked(tree: tree, w: w, force: force)
        }
    }

    /// A backup of `w` written to the folder `tree` if it changed since the
    /// last, or if `force`: its files copied from the outbox, those of older
    /// ones deleted. Under [backupLock].
    private func backupLocked(tree: String, w: Wallet, force: Bool) async -> Result<Void, Error> {
        let done: Result<Void, Error> = await io {
            let now = Engine.elapsedRealtime()
            backupLooked = now
            // What this backup holds at least: changes after it are due still.
            let stamp = await off { w.backupStamp() }
            let outbox = files.appendingPathComponent("backup-outbox")
            do {
                defer {
                    let left = try? FileManager.default.contentsOfDirectory(at: outbox, includingPropertiesForKeys: nil)
                    for file in left ?? [] { try? FileManager.default.removeItem(at: file) }
                }
                let name = try await off { try w.backupFolder() }
                let (chosen, fresh) = try await off { try Engine.folder(bookmark: tree) }
                // A stale bookmark made anew, as iOS asks: unless backups stopped or another folder was chosen since.
                if let fresh, prefs.backupFolder == tree { prefs.backupFolder = fresh }
                let folder = try await off { try self.backupDir(tree: chosen, name: name) }
                let present = try await off { try folder.children() }.filter { !$0.dir }
                    .map { FolderFile(name: $0.name, bytes: $0.bytes >= 0 ? UInt64($0.bytes) : nil) }
                let step = try await off { try w.prepareBackup(outbox: outbox.path, present: present, force: force) }
                switch step {
                // Contract work under way: the change still due.
                case .busy: return
                case .unchanged: break
                case .write(let run):
                    for name in run.write {
                        try await off { try folder.write(name: name, from: outbox.appendingPathComponent(name)) }
                    }
                    for name in run.delete {
                        do {
                            try await off { try folder.delete(name: name) }
                        } catch {
                            AppLog.warn(line: "an old backup's file", e: error)
                        }
                    }
                    try await off { try w.backupPublished(generation: run.generation) }
                    prefs.backupAt = Engine.currentTimeMillis()
                    AppLog.info(line: "backup written: \(run.write.count) files, \(run.delete.count) deleted")
                }
            }
            backedStamp = stamp
            backupRetry = 0
            update { $0.backup = AutoBackup(at: prefs.backupAt) }
        }
        if case .failure(let e) = done {
            AppLog.warn(line: "an automatic backup", e: e)
            // Tried again in a while, not every round.
            backupRetry = Engine.elapsedRealtime() + Engine.BACKUP_RETRY_MS
            update { $0.backup = AutoBackup(at: prefs.backupAt, problem: explain(e: e)) }
        }
        return done
    }

    /// The folder the wallet's backups are in: `tree` itself if the user
    /// chose that one, else the one of its name in it, made if not there.
    /// Backing up and restoring find the same.
    private nonisolated func backupDir(tree: URL, name: String) throws -> Folder {
        let root = Folder.of(tree: tree)
        return try root.name() == name ? root : root.dir(name: name)
    }

    /// Automatic backups to the folder `tree` the user chose, the first one written now.
    func startBackups(tree: URL) async -> Result<Void, Error> {
        // Kept as a bookmark: iOS reaches the folder through it once the app
        // restarts, and holds no grant of one kept before to give back.
        let kept: String
        do {
            kept = try await off { try Engine.bookmark(tree: tree) }
        } catch {
            return .failure(error)
        }
        prefs.backupFolder = kept
        AppLog.info(line: "automatic backups on")
        return await backupNow(tree: kept, force: true)
    }

    /// The folder `tree` as [Prefs.backupFolder] keeps it: a bookmark of it,
    /// through which iOS reaches it again once the app restarts.
    private nonisolated static func bookmark(tree: URL) throws -> String {
        #if os(iOS)
        try reaching(url: tree) {
            try tree.bookmarkData(options: .minimalBookmark, includingResourceValuesForKeys: nil, relativeTo: nil)
        }.base64EncodedString()
        #else
        tree.path
        #endif
    }

    /// The folder [bookmark] keeps as `bookmark`, and a bookmark of it made
    /// anew if iOS finds that one stale.
    private nonisolated static func folder(bookmark: String) throws -> (URL, String?) {
        #if os(iOS)
        var stale = false
        let url = try URL(resolvingBookmarkData: Data(base64Encoded: bookmark) ?? Data(), bookmarkDataIsStale: &stale)
        return try (url, stale ? Engine.bookmark(tree: url) : nil)
        #else
        (URL(fileURLWithPath: bookmark), nil)
        #endif
    }

    /// `call` with `url`, a file or folder the user chose, open: iOS lets the
    /// app reach one outside its own only while it is.
    private nonisolated static func reaching<T>(url: URL, call: () throws -> T) rethrows -> T {
        #if os(iOS)
        let open = url.startAccessingSecurityScopedResource()
        defer { if open { url.stopAccessingSecurityScopedResource() } }
        #endif
        return try call()
    }

    /// A backup written now, changed or not.
    func backupAgain() async -> Result<Void, Error> {
        guard let tree = prefs.backupFolder else {
            return .failure(LicashError.Invalid(reason: "no backup folder chosen"))
        }
        return await backupNow(tree: tree, force: true)
    }

    /// No more automatic backups: those in the folder stay.
    func stopBackups() {
        // Its bookmark holds no grant to give back.
        prefs.backupFolder = nil
        prefs.backupAt = nil
        update { $0.backup = nil }
        AppLog.info(line: "automatic backups off")
    }

    /// A step of catching up logged, once a start, with the time it took since the light client started.
    private func phase(step: String) {
        let first = phases.insert(step.components(separatedBy: " to block")[0]).inserted
        if first && lightAt > 0 {
            AppLog.info(line: "phase: \(step), \((Engine.elapsedRealtime() - lightAt) / 1000) s in")
        }
    }

    private func stopLight() async {
        if let light { try? await off { try light.stop() } }
        light = nil
        lightOf = nil
    }

    private func loop() async {
        while true {
            if foreground || keepers > 0 {
                findProviders()
                if foreground { awaitMail() }
                do {
                    try await sync()
                } catch {
                    AppLog.warn(line: "sync", e: error)
                    update { $0.problem = explain(e: error) }
                }
                prefetchQuotes()
                if ui.chain.synced { rounds += 1 }
                if foreground { keepCatchingUp() }
                await autoBackup()
            }
            // Out of sight, what is watched waits for blocks: no hurry,
            // unless catching up.
            let ms =
                if !foreground && !behind(ui: ui) {
                    Engine.AWAY_MS
                } else if !foreground {
                    Engine.ROUND_MS
                } else if eager {
                    Engine.EAGER_MS
                } else {
                    Engine.ROUND_MS
                }
            await wake(ms: ms)
        }
    }

    private func sync() async throws {
        guard let light else { return }
        let status = try await off { try statusOf(light: light) }
        let tor = await torStatus()
        update {
            $0.chain = status
            $0.torStatus = tor
        }
        if tor?.ready == true { phase(step: "Tor ready") }
        // Nothing to ask of it before it starts.
        if status.importing >= 0 { return }
        guard wallet != nil else { return }
        var hubs = try await local { try $0.hubs() }
        if hubs.isEmpty { return }
        if !hubsLearned || Engine.currentTimeMillis() - hubsLearnedAt > Engine.HUBS_MS {
            try await learnHubs(hubs: hubs)
            hubs = try await local { try $0.hubs() }
        }
        let chain = GoChain(light: light)
        var problems: [String] = []
        func failed(step: String, e: LicashError) {
            AppLog.warn(line: step, e: e)
            problems.append(explain(e: e))
        }
        var quiet = status
        quiet.headers = 0
        quiet.peerHeight = 0
        if quiet != logged {
            logged = quiet
            AppLog.info(
                line: "block \(status.height), \(status.peers) peers, " + (status.synced ? "synced" : "syncing")
            )
        }
        var caughtUp = false
        if status.synced {
            var before: UInt32? = nil
            while true {
                let scan: Scan
                do {
                    scan = try await locked { try $0.scan(chain: chain) }
                } catch let e as LicashError {
                    failed(step: "looking for payments on Bitcoin", e: e)
                    // A hub may serve other blocks now: learn it next round.
                    hubsLearned = false
                    break
                }
                // The balance shown with the coins found, not after the rest
                // of the round (the mailbox alone can take half a minute):
                // caught up showing 0 would look like nothing was found.
                if scan.found > 0 {
                    AppLog.info(line: "found \(scan.found) coins of \(scan.foundValue) sat to block \(scan.next &- 1)")
                    await refresh()
                }
                update { $0.scan = scan }
                if scan.next > scan.height {
                    caughtUp = true
                    if !prefs.caughtUp { prefs.caughtUp = true }
                    update { $0.caughtUp = true }
                    phase(step: "payments looked for to block \(scan.height)")
                    break
                }
                // The hubs' tweaks lag the chain: on next round.
                if scan.next == before { break }
                before = scan.next
                AppLog.info(line: "looked in to block \(scan.next &- 1) of \(scan.height)")
            }
        }
        // Not while looking in blocks again: what it forgot would show as
        // on its way until found again.
        if caughtUp || !status.synced {
            do {
                let incoming = try await locked { try $0.incoming(chain: chain) }
                update { $0.incoming = incoming }
            } catch let e as LicashError {
                failed(step: "payments on their way", e: e)
            }
        }
        do {
            try await locked { try $0.resume() }
        } catch let e as LicashError {
            failed(step: "finishing what is under way", e: e)
        }
        // With the chain as it is: a claim waits for the provider's coin.
        if status.synced {
            do {
                _ = try await locked { try $0.followPayouts(chain: chain) }
            } catch let e as LicashError {
                failed(step: "paying out on Bitcoin", e: e)
            }
            resumeContracts(chain: chain)
            // Its balance looked for, and a withdrawal made if due.
            do {
                _ = try await locked { try $0.runCold(chain: chain) }
            } catch let e as LicashError {
                AppLog.info(line: "cold storage deferred: \(explain(e: e))")
            }
        }
        if hubs.contains(where: { $0.mailbox }) {
            let reached: Bool
            do {
                _ = try await collect()
                reached = true
            } catch let e as LicashError {
                failed(step: "the mailbox", e: e)
                reached = false
            }
            // Full: newer mail, junk anyone may send, may hide a payment there.
            let full = reached ? try await !locked { $0.mailboxFull() }.isEmpty : false
            if full { AppLog.info(line: "a mailbox relay answered all the mail asked for: a payment may be hidden") }
            update {
                $0.relaysReached = reached
                $0.relaysFull = full
                $0.relaysMs = reached ? collectedMs : nil
            }
        }
        // Only with every coin found: a partial look could move part.
        if caughtUp {
            do {
                try await offer()
            } catch let e as LicashError {
                failed(step: "looking for protection to offer", e: e)
            }
        }
        // Common SDK guards handle privacy rest, fee ceilings and the
        // daily budget. Failure here does not fail completed sync work.
        do {
            _ = try await locked { try $0.maintainNotes() }
        } catch let e as LicashError {
            AppLog.info(line: "note maintenance deferred: \(explain(e: e))")
        }
        update { $0.problem = problems.first }
        await refresh()
        tell()
    }

    /// Take the payments in the mailbox, each told of at once, their notes
    /// unlinked after: whether there were any.
    /// How long the last look in the mailbox finding nothing took, its relays
    /// asked, in milliseconds: one finding payments also redeems them at the
    /// hub, seconds over Tor that are not the relays'.
    private var collectedMs: Int64?

    private func collect() async throws -> Bool {
        let (got, took) = try await locked { w in
            let asked = Engine.elapsedRealtime()
            let got = try w.collectNow()
            return (got, Engine.elapsedRealtime() - asked)
        }
        if got.isEmpty { collectedMs = took }
        for one in got {
            AppLog.info(line: "received \(one.amount) sat in the mailbox")
            emit(event: Event(text: "Received \(sat(amount: one.amount))", received: one.amount))
        }
        return !got.isEmpty
    }

    /// Look in the mailbox every [MAIL_MS] while someone waits to be paid, in
    /// sight, apart from the rounds and what they wait on.
    private func watchMailbox() {
        if watchingMailbox { return }
        watchingMailbox = true
        Task {
            while eager && foreground {
                let hubs = wallet == nil ? nil : try? await local { try $0.hubs() }
                if let hubs, hubs.contains(where: { $0.mailbox }) {
                    do {
                        if try await collect() { await refresh() }
                    } catch {
                        AppLog.warn(line: "the mailbox, looked in apart", e: error)
                    }
                }
                try? await Task.sleep(for: .milliseconds(Engine.MAIL_MS))
            }
            watchingMailbox = false
            // Asked again as it stopped.
            if eager && foreground { watchMailbox() }
        }
    }

    /// In sight, take the mail as the mailbox's relays push it, not at the
    /// next round: they stay connected once looked in, a wait ([PUSH_WAIT_S]
    /// at most) ending as they push. Not holding the wallet while waiting.
    private func awaitMail() {
        if awaitingMail { return }
        awaitingMail = true
        Task {
            while foreground {
                guard let wallet else { break }
                let wait = Engine.PUSH_WAIT_S
                let pushed = await off { wallet.waitForMail(seconds: wait) }
                if !pushed || !foreground { continue }
                AppLog.info(line: "the mailbox pushed mail")
                do {
                    if try await collect() { await refresh() }
                } catch {
                    AppLog.warn(line: "the mailbox, as pushed", e: error)
                }
            }
            awaitingMail = false
        }
    }

    /// Learn again what each hub trusted serves: it may have changed since added.
    private func learnHubs(hubs: [Hub]) async throws {
        var reached = ui.hubsReached.filter { url, _ in hubs.contains { $0.url == url } }
        var took = ui.hubsMs.filter { url, _ in hubs.contains { $0.url == url } }
        for hub in hubs {
            // One not answering lately passed over at once, not waited on
            // again with the wallet held.
            do {
                _ = try await locked { try $0.relearnHub(url: hub.url) }
                reached[hub.url] = true
            } catch let e as LicashError {
                AppLog.warn(line: "learning what \(hub.url) serves", e: e)
                reached[hub.url] = false
            }
            // Its fastest answer lately: a round trip, not a circuit built.
            let ms = try await local { $0.hubAnswersMs(url: hub.url) }
            took[hub.url] = ms.map { Int64(bitPattern: $0) }
            // Said as each answers, or not: not once the slowest has.
            update {
                $0.hubsReached = reached
                $0.hubsMs = took
            }
        }
        hubsLearned = true
        hubsLearnedAt = Engine.currentTimeMillis()
    }

    /// Ask each hub trusted that opens contracts for its quote ahead, in
    /// the background, once the last is older than [QUOTE_MS] or what is
    /// on Bitcoin changed: protecting shows at once ([contractQuote]).
    private func prefetchQuotes() {
        if !foreground || !ui.chain.synced || ui.opening != nil { return }
        if quoting { return }
        quoting = true
        Task {
            do {
                let onchain = try await local { try $0.balance() }.onchain
                let now = Engine.currentTimeMillis()
                for hub in try await local(call: { try $0.hubs() }).filter({ $0.contracts != nil }) {
                    if let cached = quotes[hub.url], cached.1 == onchain, now - cached.2 < Engine.QUOTE_MS / 2 { continue }
                    do {
                        let quote = try await local { try $0.contractQuote(hub: hub.url) }
                        quotes[hub.url] = (quote, onchain, Engine.currentTimeMillis())
                    } catch {
                        AppLog.info(line: "asking \(hub.url) for a quote ahead: \(error.localizedDescription)")
                    }
                }
            } catch {
                AppLog.warn(line: "asking for quotes ahead", e: error)
            }
            quoting = false
        }
    }

    /// Look for a contract to offer for what was paid on Bitcoin, to protect
    /// it once in the balance ([Ui.offer]): again once what is there
    /// changes, or every [OFFER_MS]. None for what was cashed out to this
    /// wallet, which stays, nor while one is being opened.
    private func offer() async throws {
        let balance = try await local { try $0.balance() }
        if balance.onchain <= balance.kept || ui.opening != nil {
            update { $0.offer = nil }
            return
        }
        let now = Engine.currentTimeMillis()
        if balance.onchain == offeredFor && now - offeredAt < Engine.OFFER_MS { return }
        let offer: ContractQuote?
        do {
            offer = try await local { try $0.contractOffer() }
        } catch let e as LicashError {
            AppLog.info(line: "no contract to offer now: \(explain(e: e))")
            return
        }
        offeredFor = balance.onchain
        offeredAt = now
        AppLog.info(line: offer.map { "offering a contract at \($0.hub) of \($0.suggestedCap) sat" } ?? "no contract to offer")
        let declined = offerDeclined(q: offer, declined: prefs.offerDeclined)
        update {
            $0.offer = offer
            $0.offerDeclined = declined
        }
    }

    /// Not to offer `q` again unasked: its coin's offers are declined.
    func declineOffer(q: ContractQuote) {
        guard let coin = offeredCoin(q: q) else { return }
        prefs.offerDeclined = coin
        update { $0.offerDeclined = true }
    }

    /// Look for the swap providers announced, apart from the rounds, for a
    /// quote to ask them without waiting on relays: every [PROVIDERS_MS]
    /// once looked for, [PROVIDERS_RETRY_MS] after failing to, at once for
    /// a wallet opened since. In sight, those found are asked ahead every
    /// [WARM_MS], for a quote to find their connections open.
    private func findProviders() {
        guard let wallet else { return }
        let due = providersDue
        let now = Engine.nanoTime()
        let look = due.map { $0.0 !== wallet || now >= $0.1 } ?? true
        let warmed = providersWarmed
        let warm = foreground && (warmed.map { $0.0 !== wallet || now >= $0.1 } ?? true)
        if !look && !warm { return }
        if findingProviders { return }
        findingProviders = true
        Task {
            if look {
                let wait: Int64
                do {
                    _ = try await off { try wallet.refreshProviders() }
                    wait = Engine.PROVIDERS_MS
                } catch {
                    AppLog.warn(line: "looking for swap providers", e: error)
                    wait = Engine.PROVIDERS_RETRY_MS
                }
                providersDue = (wallet, Engine.nanoTime() + wait * 1_000_000)
            }
            // Never by a wallet replaced since: it may reach them another way than Tor is set to.
            if foreground && self.wallet === wallet {
                do {
                    _ = try await off { try wallet.warmProviders() }
                } catch {
                    AppLog.info(line: "asking swap providers ahead: \(error.localizedDescription)")
                }
                providersWarmed = (wallet, Engine.nanoTime() + Engine.WARM_MS * 1_000_000)
            }
            findingProviders = false
        }
    }

    /// Carry on with the contracts being opened, watch the funding of those
    /// signed and carry on those forced closed, in the background: an
    /// opening carried on downloads for minutes. Nothing to do once each is
    /// confirmed or given up, and each forced closed over.
    private func resumeContracts(chain: GoChain) {
        let ui = self.ui
        let waiting = ui.contracts.contains { carriedOn(contract: $0) } || ui.recoveries.contains { carriedOn(recovery: $0) }
        let protecting = ui.contracts.contains { $0.state == .protecting }
        if !(waiting || protecting) || contracting { return }
        contracting = true
        Task {
            do {
                if waiting { _ = try await local { try $0.resumeContracts(chain: chain) } }
            } catch {
                // Not to bring the app down from a task of its own.
                AppLog.warn(line: "carrying on with contracts", e: error)
            }
            do {
                // As she consented, else only told: nothing done without.
                if protecting {
                    let margin = exitMarginBlocks(days: prefs.exitMarginDays)
                    let exits = try await local { try $0.driveExits(chain: chain, exitMargin: margin) }
                    for e in exits {
                        if let error = e.error { AppLog.warn(line: "exit of contract \(e.id), \(e.step): \(error)") }
                    }
                    update { $0.exits = exits }
                }
            } catch {
                AppLog.warn(line: "exiting contracts ending", e: error)
            }
            contracting = false
            await refresh()
        }
    }

    /// Tell of coins found on Bitcoin and payments on their way not told of
    /// yet: none of those there when the app started, nor coins found again
    /// after looking in blocks again.
    private func tell() {
        let ui = self.ui
        let coins = ui.history.filter { $0.kind == .onchain }
        var told = self.told ?? Set((coins + ui.incoming).map { $0.id })
        let arrived = coins.filter { told.insert($0.id).inserted }.reduce(0) { $0 + $1.amount }
        if arrived > 0 { emit(event: Event(text: "\(sat(amount: arrived)) arrived on Bitcoin", received: arrived)) }
        let coming = ui.incoming.filter { told.insert($0.id).inserted }.reduce(0) { $0 + $1.amount }
        self.told = told
        if coming > 0 { emit(event: Event(text: "\(sat(amount: coming)) on its way on Bitcoin")) }
    }

    private func refresh() async {
        var shown: Shown?
        if let w = wallet {
            // A read failing logged, what is shown kept.
            func read<T>(failing what: String, or kept: T, call: @escaping @Sendable () throws -> T) async -> T {
                do {
                    return try await off(call: call)
                } catch {
                    AppLog.warn(line: what, e: error)
                    return kept
                }
            }
            do {
                let contracts = await read(failing: "reading the contracts", or: ui.contracts) { try w.contracts() }
                let recoveries = await read(failing: "reading the forced closes", or: ui.recoveries) { try w.recoveries() }
                let history = try await off { try w.history() }
                // From the wallet alone, no network: Home tells each step.
                let toRead = detailsToRead(history: history)
                let under = await off {
                    var under: [String: Details] = [:]
                    for a in toRead {
                        if let d = try? w.details(kind: a.kind, id: a.id) { under[a.key] = d }
                    }
                    return under
                }
                let raised = await read(failing: "reading the fees agreed", or: ui.raised) { try w.feesRaised() }
                let noteMaintenance = await read(failing: "reading future note work", or: ui.noteMaintenance) {
                    try w.noteMaintenanceNeeded()
                }
                let onTheirWay = await read(failing: "reading the payments on their way", or: ui.onTheirWay) {
                    try w.onTheirWay()
                }
                let cold = await read(failing: "reading cold storage", or: ui.cold) { try w.cold() }
                // Only the lines a contact could be named on: payments made, and those whose payer shared a code.
                let named = history.filter { $0.kind == .sent || $0.from != nil }
                let contacts = await read(failing: "reading the contacts", or: ui.contacts) {
                    let names = try w.contactsOf(lines: named)
                    return Dictionary(
                        zip(named, names).compactMap { a, name in name.map { (a.key, $0) } },
                        uniquingKeysWith: { $1 }
                    )
                }
                let (network, balance, hubs, tor) = try await off { try (w.network(), w.balance(), w.hubs(), w.tor()) }
                shown = Shown(
                    network: network, balance: balance, history: history, hubs: hubs, raised: raised, tor: tor,
                    contracts: contracts, recoveries: recoveries, under: under, noteMaintenance: noteMaintenance,
                    onTheirWay: onTheirWay, cold: cold, contacts: contacts
                )
            } catch {
                AppLog.warn(line: "reading the wallet", e: error)
            }
        }
        update {
            if wallet == nil {
                $0.wallet = false
                $0.noteMaintenance = false
                $0.network = prefs.network
                $0.developer = prefs.developer
                $0.tor = prefs.tor
            } else if let shown {
                $0.wallet = true
                $0.network = shown.network
                $0.developer = prefs.developer
                $0.balance = shown.balance
                $0.history = shown.history
                $0.hubs = shown.hubs
                $0.raised = shown.raised
                $0.tor = shown.tor
                $0.contracts = shown.contracts
                $0.recoveries = shown.recoveries
                $0.under = shown.under
                $0.noteMaintenance = shown.noteMaintenance
                $0.asked = prefs.askedAt
                $0.onTheirWay = shown.onTheirWay
                $0.cold = shown.cold
                $0.contacts = shown.contacts
                $0.backup = prefs.backupFolder == nil ? nil : $0.backup ?? AutoBackup(at: prefs.backupAt)
            }
        }
        if let shown {
            // Written down as it changes, for Stats. A failure costs only that history.
            let balance = shown.balance
            Task {
                guard let w = wallet else { return }
                do {
                    try await off { try w.recordBalance(balance: balance) }
                } catch {
                    AppLog.warn(line: "writing the balance down", e: error)
                }
            }
            watch()
        }
    }

    /// After each look at the wallet: out of sight, tell of the payments
    /// finished and of payouts nearing their deadline, and keep watching,
    /// the service started while in sight, as long as anything is under way.
    private func watch() {
        let ui = self.ui
        let level = watchOf(ui: ui, now: Engine.currentTimeMillis() / 1000)
        update { $0.watch = level }
        let looked = look(history: ui.history, before: prefs.watched, seen: seen)
        prefs.watched = looked.under
        seen = Set(ui.history.map { $0.key })
        let near = nearing(ui: ui)
        var warned = prefs.warned.intersection(looked.under)
        if !foreground {
            for activity in looked.finished {
                if let text = news(activity: activity) { notifier.payment(key: activity.key, text: text) }
            }
            for (key, by) in near {
                if warned.contains(key) { continue }
                notifier.deadline(
                    key: key,
                    text: "A payment on Bitcoin must be claimed by block \(digits(amount: UInt64(by))), or it fails. " +
                        "Licash claims it once open with a connection."
                )
                warned.insert(key)
            }
        }
        prefs.warned = warned
        // In sight or not: nothing else shows them. Each told once.
        var told = prefs.exitsTold
        for notice in exitNotices(exits: ui.exits, recoveries: ui.recoveries) {
            if told.contains(notice.key) { continue }
            if notice.urgent {
                notifier.deadline(key: notice.key, text: notice.text)
            } else {
                notifier.payment(key: notice.key, text: notice.text)
            }
            told.insert(notice.key)
        }
        prefs.exitsTold = told
        if level == .now && foreground { WatchService.start() }
        let periodic = level != Watch.none
        if periodic != looking {
            looking = periodic
            if periodic { WatchJob.schedule() } else { WatchJob.cancel() }
        }
    }

    private struct Shown: Equatable {
        var network: Network
        var balance: LicashCore.Balance
        var history: [Activity]
        var hubs: [Hub]
        var raised: [FeesRaised]
        var tor: Bool
        var contracts: [Contract]
        var recoveries: [Recovery]
        var under: [String: Details]
        var noteMaintenance: Bool
        var onTheirWay: UInt32
        var cold: ColdStorage?
        var contacts: [String: String]
    }

    /// A new wallet of the network chosen, born at the chain's height if known,
    /// else at the block this release pins (mined before it, so nothing of the
    /// wallet's is earlier): never scanned from the first block.
    func create() async -> Result<Void, Error> {
        await io {
            try await mutex.withLock {
                let light = light
                let network = prefs.network
                let tor = try await self.tor()
                let dir = walletDir
                let (made, birthday) = try await off {
                    let pinned = UInt32(bitPattern: MobilePinHeight(chainName(network: network)))
                    var birthday = pinned
                    if let light, light.synced() {
                        var height: Int32 = 0
                        try light.height(&height)
                        birthday = UInt32(bitPattern: height)
                    }
                    return (try Wallet.create(dir: dir, network: network, birthday: birthday, tor: tor), birthday)
                }
                wallet = await configured(w: made)
                AppLog.info(line: "wallet made at block \(birthday)")
            }
            // Running since the app started, of this network: shown at once,
            // not waiting on it.
            Task { await lightLock.withLock { await startLight() } }
            await refresh()
            poke()
        }
    }

    /// The 24 words restoring this wallet's keys: secret, shown only when asked, never logged.
    func backupPhrase() async -> Result<[String], Error> {
        await io { try await local { try $0.backupPhrase() }.components(separatedBy: " ") }
    }

    /// Everything this wallet holds as it is now, saved `to` a file the user
    /// chose: unencrypted, private. Written whole to the cache first, so a
    /// failure leaves no half a backup behind as if it were one.
    func exportBackup(to: URL) async -> Result<Void, Error> {
        await io {
            let made = cacheDir.appendingPathComponent("backup-\(Engine.nanoTime()).licash")
            defer { try? FileManager.default.removeItem(at: made) }
            try await locked { try $0.exportStateBundle(to: made.path) }
            try await off {
                try Engine.reaching(url: to) {
                    guard FileManager.default.createFile(atPath: to.path, contents: nil) else {
                        throw LicashError.Invalid(reason: "that file cannot be written")
                    }
                    let input = try FileHandle(forReadingFrom: made)
                    defer { try? input.close() }
                    let out = try FileHandle(forWritingTo: to)
                    defer { try? out.close() }
                    while let chunk = try input.read(upToCount: 1 << 16), !chunk.isEmpty {
                        try out.write(contentsOf: chunk)
                    }
                }
            }
            let bytes = (try? made.resourceValues(forKeys: [.fileSizeKey]))?.fileSize ?? 0
            AppLog.info(line: "full backup saved, \(bytes) bytes")
        }
    }

    /// A wallet of the network chosen from its 24 words: its keys, and its
    /// coins once Bitcoin is looked through from `birthday`, the block
    /// written down with them, or from the first block if none.
    func restorePhrase(words: [String], birthday: UInt32?) async -> Result<Void, Error> {
        await restored {
            let tor = try await self.tor()
            let (dir, phrase, network) = (walletDir, words.joined(separator: " "), prefs.network)
            return try await off {
                try Wallet.fromPhrase(dir: dir, phrase: phrase, network: network, birthday: birthday, tor: tor)
            }
        }
    }

    /// The block the wallet's coins on Bitcoin are looked for from, once known.
    func birthday() async -> Result<UInt32?, Error> { await io { try await local { $0.birthday() } } }

    /// How the light client and Tor stand now, for a screen showing them: not waiting for the next round.
    func live() async {
        var chain: ChainStatus?
        if let light { chain = try? await off { try statusOf(light: light) } }
        let tor = await torStatus()
        update {
            $0.chain = chain ?? $0.chain
            $0.torStatus = tor
        }
    }

    /// How the app's Tor client stands, over Tor. It starts connecting if it is not.
    private func torStatus() async -> TorStatus? {
        guard let client = try? await tor() else { return nil }
        return await off { client.status() }
    }

    /// Start Tor afresh, as asked: what it learned of the Tor network (its
    /// state and directory cache) forgotten, the wallet opened again and
    /// the light client started again through it. Nothing the wallet holds
    /// is touched.
    func resetTor() async -> Result<Void, Error> {
        await io {
            try await lightLock.withLock {
                try await mutex.withLock {
                    // The light client and the wallet reach everything through Tor.
                    await stopLight()
                    wallet = nil
                    await closeTor()
                    // Where it is kept, and where an earlier version kept it.
                    let dirs = [files.appendingPathComponent("tor"), URL(fileURLWithPath: walletDir).appendingPathComponent("tor")]
                    await off {
                        for dir in dirs {
                            for part in ["state", "cache"] {
                                try? FileManager.default.removeItem(at: dir.appendingPathComponent(part))
                            }
                        }
                    }
                    AppLog.info(line: "Tor's state forgotten")
                    if walletExists(dir: walletDir) {
                        let tor = try await self.tor()
                        let dir = walletDir
                        let opened = try await off { try Wallet.open(dir: dir, tor: tor) }
                        wallet = await configured(w: opened)
                    }
                }
                update { $0.torStatus = nil }
                await startLight()
            }
            await refresh()
            poke()
        }
    }

    /// The wallet a full backup `from` holds, of its own network. Never over a wallet there.
    func restoreBackup(from: URL) async -> Result<Void, Error> {
        let copy = cacheDir.appendingPathComponent("restore-\(Engine.nanoTime()).licash")
        defer { try? FileManager.default.removeItem(at: copy) }
        let copied = await io {
            let most = Int64(bitPattern: stateBundleMost())
            let whole = try await off {
                try Engine.reaching(url: from) {
                    guard let input = InputStream(url: from) else {
                        throw LicashError.Invalid(reason: "that file cannot be read")
                    }
                    return try copyAtMost(input: input, to: copy, most: most)
                }
            }
            guard whole else { throw LicashError.Invalid(reason: "that file is too large to be a backup") }
        }
        switch copied {
        case .success:
            return await restored {
                let tor = try await self.tor()
                let dir = walletDir
                return try await off { try Wallet.fromStateBundle(dir: dir, bundle: copy.path, tor: tor) }
            }
        case .failure(let e):
            return .failure(e)
        }
    }

    /// Where a backup folder's files are copied to, to restore from.
    private var restoreCopy: URL { files.appendingPathComponent("restore-folder") }

    /// The newest backup of the wallet of `words`, on the network chosen, in
    /// the folder `tree` or in its own folder in it: none if none restores.
    /// Its files are copied to the phone, kept for [restoreFolder] once one
    /// is found, else gone. A file too large for a backup is never copied.
    func findFolderBackup(tree: URL, words: [String]) async -> Result<BackupFound?, Error> {
        await io {
            let phrase = words.joined(separator: " ")
            let network = prefs.network
            let name = try await off { try backupFolderName(phrase: phrase, network: network) }
            let root = Folder.of(tree: tree)
            // As [backupDir] finds it, never made here.
            let folder = try await off { try root.name() == name ? root : root.find(name: name) }
            let copy = restoreCopy
            try? FileManager.default.removeItem(at: copy)
            try? FileManager.default.createDirectory(at: copy, withIntermediateDirectories: true)
            var found: BackupFound?
            defer { if found == nil { try? FileManager.default.removeItem(at: copy) } }
            if let folder {
                let most = Int64(bitPattern: backupFileMost())
                for c in try await off(call: { try folder.children() }) {
                    if c.dir || !c.name.hasSuffix(".lcb") { continue }
                    let read = try await off { try folder.read(child: c, to: copy.appendingPathComponent(c.name), most: most) }
                    if !read { AppLog.warn(line: "\(c.name) passed over: too large for a backup") }
                }
                found = try await off { try findBackup(phrase: phrase, network: network, files: copy.path) }
            }
            return found
        }
    }

    /// The wallet of `words` from its backup of `generation` found by
    /// [findFolderBackup], its automatic backups going on to `tree`.
    func restoreFolder(tree: URL, words: [String], generation: UInt64) async -> Result<Void, Error> {
        let phrase = words.joined(separator: " ")
        let done = await restored {
            let tor = try await self.tor()
            let (dir, network, copied) = (walletDir, prefs.network, restoreCopy.path)
            return try await off {
                try Wallet.fromBackup(
                    dir: dir, phrase: phrase, network: network, files: copied, generation: generation, tor: tor
                )
            }
        }
        let copy = restoreCopy
        await off { try? FileManager.default.removeItem(at: copy) }
        if case .success = done, case .failure(let e) = await startBackups(tree: tree) {
            AppLog.warn(line: "backups after a restore", e: e)
        }
        return done
    }

    /// The wallet `open` makes, there being none: its network the app's from
    /// now on, and Tor as that network has it unless chosen.
    private func restored(open: () async throws -> Wallet) async -> Result<Void, Error> {
        await io {
            try await mutex.withLock {
                guard wallet == nil && !walletExists(dir: walletDir) else {
                    throw LicashError.Invalid(reason: "a wallet is here already")
                }
                var w = try await open()
                prefs.network = await off { [w] in w.network() }
                if await off(call: { [w] in w.tor() }) != prefs.tor {
                    // The one made is closed as it is replaced.
                    let tor = try await self.tor()
                    let dir = walletDir
                    w = try await off { try Wallet.open(dir: dir, tor: tor) }
                }
                wallet = await configured(w: w)
                hubsLearned = false
                AppLog.info(line: "wallet restored, of \(chainName(network: prefs.network))")
            }
            await lightLock.withLock { await startLight() }
            await refresh()
            poke()
        }
    }

    /// Look through Bitcoin again, as asked: the wallet from its first block,
    /// the light client's blocks fetched afresh. Notes, contracts and what is
    /// under way are untouched. Refused, nothing changed, if the wallet's
    /// first block cannot be known yet.
    func resetChain() async -> Result<Void, Error> {
        await io {
            try await lightLock.withLock {
                try await mutex.withLock {
                    if let w = wallet {
                        let chain: ChainSource = light.map { GoChain(light: $0) } ?? NoChain.instance
                        let from = try await off { try w.rescanChain(chain: chain) }
                        AppLog.info(line: "looking through Bitcoin again" + (from.map { " from block \($0)" } ?? ""))
                    }
                    await stopLight()
                }
                var network = prefs.network
                if let w = wallet { network = await off { w.network() } }
                let dir = files.appendingPathComponent("chain-\(chainName(network: network))")
                await off { try? FileManager.default.removeItem(at: dir) }
                AppLog.info(line: "light client's blocks forgotten")
                update {
                    $0.chain = ChainStatus()
                    $0.scan = nil
                }
                await startLight()
            }
            await refresh()
            poke()
        }
    }

    func addHub(url: String, announced: String? = nil) async -> Result<Hub, Error> {
        await io {
            let (hub, ms) = try await locked { w in
                let added = try w.addHub(url: url)
                return (added, w.hubAnswersMs(url: added.url).map { Int64(bitPattern: $0) })
            }
            // Reached: the connections say so before learnHubs asks again.
            update {
                $0.hubsReached[hub.url] = true
                $0.hubsMs[hub.url] = ms
            }
            // Kept to name it by in the list of hubs, its address under it.
            if let announced, let name = shownName(announced: announced) { prefs.nameHub(url: hub.url, name: name) }
            await refresh()
            poke()
            return hub
        }
    }

    /// The name the hub at `url` announced itself under when added, if any.
    func hubName(url: String) -> String? { prefs.hubName(url: url) }

    /// What the hub at [hub] charges beyond what was agreed, as the wallet
    /// last heard: none if no more. A payment refused for it asked anew.
    func feesRaised(hub: String) async -> Result<FeesRaised?, Error> {
        await io { try await local { w in try w.feesRaised().first { $0.hub == hub } } }
    }

    /// Agree to pay a hub what [raised] shows it charges, and no more.
    func approveFees(raised: FeesRaised) async -> Result<Void, Error> {
        await io {
            try await locked { try $0.approveFees(hub: raised.hub, charged: raised.charged) }
            await refresh()
        }
    }

    func optimizationQuote(hub: String) async -> Result<NoteOptimization?, Error> {
        await io { try await local { try $0.optimizationQuote(hub: hub) } }
    }

    /// The shown fee is a hard cap, including if inventory changed since.
    func optimizeNotes(quote: NoteOptimization) async -> Result<NoteOptimization?, Error> {
        let optimized = await io { try await locked { try $0.optimizeNotes(hub: quote.hub, maxFee: quote.fee) } }
        await refresh()
        return optimized
    }

    func code(amount: UInt64?) async -> Result<Code, Error> {
        await io { try await local { try $0.paymentCode(amount: amount) } }
    }

    /// The wallet's extra codes: codes no one can tell are its, or each other's.
    func codes() async -> Result<[ExtraCode], Error> { await io { try await local { try $0.codes() } } }

    /// A new extra code, called `label`.
    func addCode(label: String) async -> Result<ExtraCode, Error> {
        await io { try await local { try $0.addCode(label: label) } }
    }

    /// Extra code `index`, asking `amount` if given.
    func extraCode(index: UInt32, amount: UInt64?) async -> Result<Code, Error> {
        await io { try await local { try $0.extraCode(index: index, amount: amount) } }
    }

    /// The code on Bitcoin: an address any wallet pays, handed out, and watched from then on.
    func bitcoinCode(amount: UInt64?) async -> Result<Code, Error> {
        await io { try await local { try $0.bitcoinCode(amount: amount) } }
    }

    /// The silent payment address alone.
    func silentAddress() async -> Result<String, Error> { await io { try await local { $0.silentAddress() } } }

    /// The wallet's static address of donations, through a swap provider
    /// announced that takes them at a hub of its code: nil if none does.
    func donationAddress() async -> Result<DonationAddress?, Error> {
        await io { try await local { try $0.donationAddress() } }
    }

    /// Keep the provider of the donation address `lnurl` shown: it shows from then on.
    func keepDonationAddress(lnurl: String) async -> Result<Void, Error> {
        await io { try await local { try $0.keepDonationAddress(lnurl: lnurl) } }
    }

    /// More of a line of the history, as far as the wallet looked.
    func details(activity: Activity) async -> Result<Details, Error> {
        await io { try await local { try $0.details(kind: activity.kind, id: activity.id) } }
    }

    /// The notes a line of the history spent, and those it made the wallet's.
    func notesOf(activity: Activity) async -> Result<NotesOf, Error> {
        await io { try await local { try $0.notesOf(kind: activity.kind, id: activity.id) } }
    }

    /// What the wallet did and what it cost, for Stats: read from its records, nothing asked of the network.
    func stats() async -> Result<LicashCore.Stats, Error> {
        let chain = chainSource()
        return await io { try await local { try $0.stats(chain: chain) } }
    }

    /// What the wallet held at each of `instants`, seconds since 1970, replayed from its records.
    func balanceOver(instants: [Int64]) async -> Result<[BalancePoint], Error> {
        let chain = chainSource()
        return await io { try await local { try $0.balanceOver(instants: instants, chain: chain) } }
    }

    /// What the wallet held as it changed, for what was protected over time: none if nothing was written down yet.
    func balanceSamples() async -> Result<[BalanceSample]?, Error> {
        await io { try await local { try $0.balanceSamples() } }
    }

    /// Bitcoin as the light client tells it, or, with none running, as nothing.
    private func chainSource() -> ChainSource { light.map { GoChain(light: $0) } ?? NoChain.instance }

    /// Where to see the transaction `txid`, if anywhere.
    func explorerLink(txid: String) -> String? {
        explorerBase().map { "\($0)/tx/\(txid)" }
    }

    func explorerAddress(address: String) -> String? { explorerBase().map { "\($0)/address/\(address)" } }

    private func explorerBase() -> String? {
        var base: String?
        switch prefs.explorer {
        case "default": base = explorer(network: ui.network)
        case "": base = nil
        case let chosen: base = chosen
        }
        while base?.hasSuffix("/") == true { base?.removeLast() }
        return base
    }

    func scanned(text: String) async -> Result<Scanned, Error> {
        await io { try await local { try $0.scanned(text: text) } }
    }

    /// How paying `amount`, or what the code `code` asks, in notes goes, and
    /// all it costs: [pay] pays it so. At a hub not trusted, the payee's,
    /// visited and proving it is the one named, if no hub trusted is.
    /// What paying `code` costs now at a hub trusted holding enough, asking nothing of the network: none if no such hub.
    func payFee(code: String, amount: UInt64?) async -> Result<UInt64?, Error> {
        await io { try await local { try $0.payFee(code: code, amount: amount) } }
    }

    func payQuote(code: String, amount: UInt64?) async -> Result<PayQuote, Error> {
        await io { try await local { try $0.payQuote(code: code, amount: amount) } }
    }

    /// Every way of paying `code`: through each swap provider that could.
    func payOffers(code: String, amount: UInt64?) async -> Result<[PayOffer], Error> {
        await io { try await local { try $0.payOffers(code: code, amount: amount) } }
    }

    /// Quote paying `code` through the provider at `provider`: paid so next.
    func choosePayOffer(code: String, amount: UInt64?, provider: String) async -> Result<PayQuote, Error> {
        await io { try await local { try $0.choosePayOffer(code: code, amount: amount, provider: provider) } }
    }

    /// Pay `code` `amount`, or what it asks: answered once its hub made the
    /// payment, sent to the payee's mailbox right after, in the background.
    /// With `note` for the payee, and a code of this wallet's if `share` says
    /// which (0 its own, else that extra code's number): refused at a hub
    /// that would not keep them ([LicashError.NoteUnkept]).
    func pay(code: String, amount: UInt64?, note: String? = nil, share: UInt32? = nil) async -> Result<Paid, Error> {
        await io {
            let asked = Engine.nanoTime()
            let paid = try await mutex.withLock {
                let waited = (Engine.nanoTime() - asked) / 1_000_000
                // What a payment waits for before it starts: a round under way.
                if waited >= 500 { AppLog.info(line: "paying waited \(waited) ms for the wallet") }
                return try await local { try $0.payNow(code: code, amount: amount, note: note, share: share) }
            }
            AppLog.info(line: "paid in \((Engine.nanoTime() - asked) / 1_000_000) ms")
            await refresh()
            return paid
        }
    }

    /// What paying the Lightning invoice `text` costs the cheapest way: through
    /// a hub's gateway, or through a swap provider.
    func invoiceQuote(text: String) async -> Result<InvoiceQuote, Error> {
        await io { try await local { try $0.invoiceQuote(text: text) } }
    }

    /// Every way to pay the Lightning invoice `text`, the cheapest first.
    func invoiceOffers(text: String) async -> Result<[InvoiceQuote], Error> {
        await io { try await local { try $0.invoiceOffers(text: text) } }
    }

    /// Pay the Lightning invoice `text` as `quote` says: through its hub's
    /// gateway, or through its provider for a lock of notes at its hub, for
    /// no more than the provider and the hub's lock fee quoted. Done, or
    /// under way.
    func payInvoice(text: String, quote: InvoiceQuote) async -> Result<Activity, Error> {
        await io {
            let provider = quote.provider
            let paid = try await locked { w in
                guard let provider else { return try w.payInvoiceWithin(text: text, hub: quote.hub, maxFee: quote.fee) }
                return try w.payInvoiceVia(
                    text: text, hub: quote.hub, provider: provider.url, maxFee: provider.fee, lockFee: provider.lockFee
                )
            }
            await refresh()
            poke()
            return paid
        }
    }

    /// Ask the hub taking it for least for a Lightning invoice asking the
    /// payer `amount`, its fee in it: the notes it pays the wallet, less.
    /// Where no hub takes Lightning, a swap provider that does. Saying
    /// `memo` to the payer, if given, which the hub or provider reads too.
    func askInvoiceFor(amount: UInt64, memo: String? = nil) async -> Result<LightningInvoice, Error> {
        await io {
            // Not behind a round: each invoice is advanced by one caller at a
            // time in the core, and asking makes one of its own.
            let asked = try await local { try $0.askInvoiceFor(invoiced: amount, memo: memo) }
            // Perhaps paid out of sight: looked for now and then for a while.
            prefs.askedAt = Engine.currentTimeMillis() / 1000
            await refresh()
            return asked
        }
    }

    /// Whether a swap provider has the wallet paid over Lightning at a hub
    /// it trusts: where no hub does, [askInvoiceFor] asks one.
    func providerTakesLightning() async -> Result<Bool, Error> {
        await io { try await local { try $0.providerTakesLightning() } }
    }

    /// The contacts, by name.
    func contacts() async -> Result<[Contact], Error> { await io { try await local { try $0.contacts() } } }

    /// Keep the payment code `code` as `name`'s: refused for a name another contact has.
    func saveContact(name: String, code: String) async -> Result<Contact, Error> {
        await io {
            let saved = try await local { try $0.saveContact(name: name, code: code) }
            await refresh()
            return saved
        }
    }

    /// Forget the contact named `name`.
    func removeContact(name: String) async -> Result<Void, Error> {
        await io {
            try await local { try $0.removeContact(name: name) }
            await refresh()
        }
    }

    /// The contact the payment code `code` pays, if one does.
    func contactOf(code: String) async -> Result<Contact?, Error> {
        await io { try await local { try $0.contactOf(code: code) } }
    }

    /// Where the invoice of `hash` stands: its notes taken once it is paid.
    func followInvoice(hash: String) async -> Result<LightningInvoice, Error> {
        await io {
            let followed = try await local { try $0.followInvoice(hash: hash) }
            if followed.state != .pending { await refresh() }
            return followed
        }
    }

    /// What paying `amount` to the Bitcoin address `address` costs through a
    /// swap provider, from notes at `hub` or the hub holding the most: all
    /// the notes there pay out if `amount` is nil.
    func payoutQuote(address: String, amount: UInt64?, hub: String? = nil) async -> Result<PayoutQuote, Error> {
        await io { try await local { try $0.payoutQuote(address: address, amount: amount, hub: hub) } }
    }

    /// About the most [payoutQuote] pays out to `address`, by the providers' own terms: none if none does.
    func payoutMost(address: String, hub: String?) async -> Result<UInt64?, Error> {
        await io { try await local { try $0.payoutMost(address: address, hub: hub) } }
    }

    /// The quotes of [payoutQuote] through every provider that pays it out.
    func payoutOffers(address: String, amount: UInt64?, hub: String?) async -> Result<[PayoutQuote], Error> {
        await io { try await local { try $0.payoutOffers(address: address, amount: amount, hub: hub) } }
    }

    /// An address on Bitcoin of this wallet's, to cash out to.
    func bitcoinAddress() async -> Result<String, Error> { await io { try await local { try $0.bitcoinAddress() } } }

    /// Move the largest coin on Bitcoin, whole and alone, into Licash now, one cashed out here too.
    func moveLargest() async -> Result<Activity, Error> {
        await io {
            guard let light else { throw LicashError.Unavailable(reason: "not connected to Bitcoin yet") }
            if let step = stillCatchingUp() { throw LicashError.Unavailable(reason: step) }
            let chain = GoChain(light: light)
            let moving = try await locked { try $0.moveLargestToLicash(chain: chain) }
            await refresh()
            poke()
            return moving
        }
    }

    /// Pay out as `quote` says: under way until its claim confirms. If its
    /// provider fails before the notes are locked to it, through another
    /// paying `asked` (all if nil) for no more in fees.
    func payOut(quote: PayoutQuote, asked: UInt64?) async -> Result<Activity, Error> {
        await io {
            let paying = try await locked { try $0.payOutOrAnother(quote: quote, asked: asked) }
            await refresh()
            poke()
            return paying
        }
    }

    /// What moving `amount` of the notes at hub `from` to hub `to` costs
    /// through each swap provider found, as preferred: all of them if nil.
    func moveOffers(from: String, to: String, amount: UInt64?) async -> Result<[MoveQuote], Error> {
        await io { try await local { try $0.moveOffers(from: from, to: to, amount: amount) } }
    }

    /// Move as the first of `offers` says, through the next costing no more
    /// if its provider fails before the notes are locked to it.
    func moveNotes(offers: [MoveQuote]) async -> Result<Activity, Error> {
        await io {
            let moving = try await locked { try $0.moveNotes(offers: offers) }
            await refresh()
            poke()
            return moving
        }
    }

    /// What opening a recovery contract at `hub` costs now, and the coin that would pay it.
    /// `hub`'s contract quote: the one asked ahead if fresh ([prefetchQuotes]),
    /// at once, else asked now.
    func contractQuote(hub: String) async -> Result<ContractQuote, Error> {
        await io {
            let onchain = try await local { try $0.balance() }.onchain
            if let cached = quotes[hub], cached.1 == onchain, Engine.currentTimeMillis() - cached.2 < Engine.QUOTE_MS {
                return cached.0
            }
            let quote = try await local { try $0.contractQuote(hub: hub) }
            quotes[hub] = (quote, onchain, Engine.currentTimeMillis())
            return quote
        }
    }

    func contractInspection(id: String) async -> Result<LicashCore.ContractInspection, Error> {
        await io { try await local { try $0.contractInspection(id: id) } }
    }

    /// The least a coin of hers on Bitcoin should hold before she forces a
    /// contract closed, for a challenge's fee, if she holds none: nil if she
    /// does, or while not connected to Bitcoin.
    func challengeShortfall() async -> Result<UInt64?, Error> {
        await io {
            guard let light else { return nil }
            let chain = GoChain(light: light)
            return try await local { try $0.challengeShortfall(chain: chain) }
        }
    }

    /// Planning from this wallet's signed terms. Never starts a close.
    func contractSafety(id: String) async -> Result<LicashCore.ContractSafety, Error> {
        await io {
            let margin = exitMarginBlocks(days: prefs.exitMarginDays)
            return try await local { try $0.contractSafety(id: id, exitMargin: margin, timeoutSlack: 6) }
        }
    }

    /// Open a recovery contract at `hub` for at most `maxPrice`, of the
    /// terms `chosen` within the hub's range if any, else of its fixed
    /// ones, in the background: it takes minutes, its downloads large.
    /// Told once signed, or why not. One at a time.
    func openContract(hub: String, maxPrice: UInt64, chosen: Chosen? = nil) {
        guard let light else {
            emit(event: Event(text: "Not connected to Bitcoin yet: try again in a moment."))
            return
        }
        if let step = stillCatchingUp() {
            emit(event: Event(text: step))
            return
        }
        guard ui.opening == nil else { return }
        update {
            $0.opening = hub
            $0.offer = nil
        }
        AppLog.info(line: "opening a contract at \(hub)\(chosen.map { " of \($0.cap) sat over \($0.blocks) blocks" } ?? "")")
        Task {
            do {
                let chain = GoChain(light: light)
                let opened = try await local { w in
                    guard let chosen else { return try w.openContract(chain: chain, hub: hub, blocks: nil, maxPrice: maxPrice) }
                    return try w.openChosenContract(
                        chain: chain, hub: hub, cap: chosen.cap, blocks: chosen.blocks, maxPrice: maxPrice, coin: chosen.coin
                    )
                }
                AppLog.info(line: "contract \(opened.id) signed at \(hub), funded by \(opened.funding ?? "null")")
                emit(event: Event(text: "Protection set up at \(host(url: hub)): on once confirmed"))
            } catch {
                AppLog.warn(line: "opening a contract at \(hub)", e: error)
                emit(event: Event(text: "Protection not set up. \(explain(e: error))"))
            }
            // What is left on Bitcoin is offered for afresh.
            offeredFor = nil
            update { $0.opening = nil }
            await refresh()
            poke()
        }
    }

    /// Force contract `id` closed, its hub silent, in the background: the
    /// notes at its hub, up to its cap, leave the balance, claimed on
    /// Bitcoin. Told once claimed, or why not. The result is for whoever
    /// waits on it, and it goes on, and is told, if none does.
    func forceClose(id: String) -> Task<Result<Void, Error>, Never> {
        guard let light else {
            emit(event: Event(text: "Not connected to Bitcoin yet: try again in a moment."))
            return Task { .failure(LicashError.Unavailable(reason: "not connected to Bitcoin yet: try again in a moment")) }
        }
        AppLog.info(line: "forcing contract \(id) closed")
        return Task {
            let done: Result<Void, Error>
            do {
                let chain = GoChain(light: light)
                let forced = try await local { try $0.forceCloseContract(chain: chain, id: id) }
                AppLog.info(line: "contract \(id) forced closed: \(forced.stage), \(forced.claimed) sat claimed")
                emit(event: Event(text: "Forcing closed: \(sat(amount: forced.claimed)) claimed on Bitcoin"))
                done = .success(())
            } catch {
                AppLog.warn(line: "forcing contract \(id) closed", e: error)
                emit(event: Event(text: "Not forced closed. \(explain(e: error))"))
                done = .failure(error)
            }
            await refresh()
            poke()
            return done
        }
    }

    /// Close contract `id` early with its hub, in the background: what it
    /// holds at the hub, up to its cap, and its reserve, back on Bitcoin
    /// in the hub's transaction. Told once it is signed, or why not. The
    /// result is for whoever waits on it, as [forceClose]'s.
    func closeContract(id: String) -> Task<Result<Void, Error>, Never> {
        guard let light else {
            emit(event: Event(text: "Not connected to Bitcoin yet: try again in a moment."))
            return Task { .failure(LicashError.Unavailable(reason: "not connected to Bitcoin yet: try again in a moment")) }
        }
        AppLog.info(line: "closing contract \(id) early")
        return Task {
            let done: Result<Void, Error>
            do {
                let chain = GoChain(light: light)
                let fold = try await local { try $0.closeContract(chain: chain, id: id) }
                AppLog.info(line: "contract \(id) closed early by \(fold)")
                emit(event: Event(text: "Closed with the hub: your notes and reserve come back on Bitcoin once its transaction confirms"))
                done = .success(())
            } catch {
                AppLog.warn(line: "closing contract \(id) early", e: error)
                emit(event: Event(text: "Not closed. \(explain(e: error))"))
                done = .failure(error)
            }
            await refresh()
            poke()
            return done
        }
    }

    /// Let forcing contract `id` closed pay up to `sats` in all to publish its claim, then carry it on.
    func allowPublicationFee(id: String, sats: UInt64) async -> Result<Void, Error> {
        await io {
            try await local { try $0.allowPublicationFee(id: id, sats: sats) }
            AppLog.info(line: "forcing contract \(id) closed may pay \(sats) sat to publish")
            await refresh()
            poke()
        }
    }

    /// Give up forcing contract `id` closed, its notes back: only until its request is final.
    func giveUpRecovery(id: String) async -> Result<Void, Error> {
        await io {
            try await local { try $0.giveUpRecovery(id: id) }
            AppLog.info(line: "forcing contract \(id) closed given up")
            await refresh()
        }
    }

    /// The hubs announced on the relays its hubs name and on the default
    /// ones, not used yet, and how many more did not answer.
    func findHubs() async -> Result<HubsFound, Error> {
        await io { try await local { try $0.findHubs(relays: defaultRelays(network: $0.network(), tor: $0.tor())) } }
    }

    /// What the wallet holds at each hub it trusts.
    func holdings() async -> Result<[Holding], Error> { await io { try await local { try $0.holdings() } } }

    /// What it holds at hubs it only visited, to pay a payee there.
    func visited() async -> Result<[Holding], Error> { await io { try await local { try $0.visited() } } }

    /// How many notes of each denomination it holds, at each hub.
    func notes() async -> Result<[Notes], Error> { await io { try await local { try $0.notes() } } }

    /// Take a payment handed over, trusting its hub if `trust`.
    func receive(payment: String, trust: Bool) async -> Result<Activity, Error> {
        await io {
            let got = try await locked { try $0.receive(payment: payment, trust: trust) }
            await refresh()
            return got
        }
    }

    /// Reach hubs, relays and Bitcoin through Tor, or not: the wallet opens
    /// again, and the light client starts again its new way.
    func setTor(on: Bool) async -> Result<Void, Error> {
        let reopened = await io {
            try await mutex.withLock {
                prefs.tor = on
                if wallet != nil {
                    wallet = nil
                    let tor = try await self.tor()
                    let dir = walletDir
                    let opened = try await off { try Wallet.open(dir: dir, tor: tor) }
                    wallet = await configured(w: opened)
                }
            }
        }
        await lightLock.withLock {
            await startLight()
            // Off: nothing reaches anything through it any more.
            if !on { await closeTor() }
        }
        guard case .success = reopened else { return reopened }
        await refresh()
        return .success(())
    }

    /// Renew notes a restarted hub no longer unlinks for at most `ppm` parts per million of them.
    func setRenewLimit(ppm: UInt32) async -> Result<Void, Error> {
        await io {
            await mutex.withLock {
                prefs.renewPpm = ppm
                if let w = wallet { await off { w.setRenewLimit(ppm: ppm) } }
            }
        }
    }

    /// The most the wallet pays to renew notes, in parts per million, if there is a wallet.
    func renewLimit() -> UInt32? { wallet?.renewLimit() }

    /// Her consent to exit contracts ending, if given: none before a first contract opens.
    func exitConsent() async -> Result<ExitConsent?, Error> { await io { try await local { try $0.exitConsent() } } }

    /// Consent to exit contracts ending, paying their notes out to
    /// `address`, with the exit margin set now.
    func consentToExit(address: String) async -> Result<ExitConsent, Error> {
        await io {
            let margin = exitMarginBlocks(days: prefs.exitMarginDays)
            let given = try await local { try $0.consentToExit(address: address, exitMargin: margin) }
            AppLog.info(line: "consented to exit contracts ending, margin \(margin) blocks")
            poke()
            return given
        }
    }

    /// Withdraw to cold storage, the wallet of `key` (a zpub, or an xpub of
    /// `script` "wpkh" or "tr"), as the rules say, its coins looked for
    /// from `birthday`, by default the chain's height now: looked for and
    /// withdrawn to on the next round.
    func setCold(
        key: String,
        script: String?,
        threshold: UInt64,
        keep: UInt64,
        maxFeePercent: Double,
        birthday: UInt32?
    ) async -> Result<ColdStorage, Error> {
        await io {
            let cold = try await locked {
                try $0.setCold(
                    key: key, script: script, threshold: threshold, keep: keep, maxFeePercent: maxFeePercent,
                    birthday: birthday
                )
            }
            AppLog.info(line: "cold storage set: past \(threshold) sat, keeping \(keep), at most \(maxFeePercent)% in fees")
            update { $0.cold = cold }
            poke()
            return cold
        }
    }

    /// Stop withdrawing to cold storage: what it holds stays there.
    func clearCold() async -> Result<Void, Error> {
        await io {
            try await locked { try $0.clearCold() }
            AppLog.info(line: "cold storage cleared")
            update { $0.cold = nil }
        }
    }

    /// Revoke her consent to exit: no step more taken.
    func revokeExitConsent() async -> Result<Void, Error> {
        await io {
            try await local { try $0.revokeExitConsent() }
            AppLog.info(line: "consent to exit contracts revoked")
        }
    }

    /// The exit margin set to `days`, her consent to exit given again with it if given.
    func setExitMargin(days: Int) async -> Result<Void, Error> {
        await io {
            prefs.exitMarginDays = days
            let margin = exitMarginBlocks(days: days)
            if let consent = try await local(call: { try $0.exitConsent() }) {
                _ = try await local { try $0.consentToExit(address: consent.address, exitMargin: margin) }
            }
        }
    }

    /// The wallet `w` opened, as its owner set it: what its renewals may cost.
    private func configured(w: Wallet) async -> Wallet {
        if let ppm = prefs.renewPpm { await off { w.setRenewLimit(ppm: ppm) } }
        // Its coins on Bitcoin looked for while the phone rests, too.
        ScanJob.schedule()
        return w
    }

    func setDeveloper(on: Bool) {
        prefs.developer = on
        update { $0.developer = on }
    }

    /// The network of the wallet to make, its light client started now.
    func setNetwork(network: Network) async -> Result<Void, Error> {
        await io {
            try await mutex.withLock {
                // A wallet there but failing to open keeps its network, and so
                // the Tor default that follows it.
                guard !walletExists(dir: walletDir) else {
                    throw LicashError.Invalid(reason: "the wallet's network is chosen once")
                }
                prefs.network = network
            }
            await lightLock.withLock { await startLight() }
            await refresh()
        }
    }

    /// The light client's peers and block-dn server, from now on.
    func setChain(peers: String, cdn: String) async -> Result<Void, Error> {
        await io {
            await lightLock.withLock {
                prefs.peers = peers
                prefs.cdn = cdn
                await startLight()
            }
            poke()
        }
    }

    /// Forget the wallet: for developers, on test networks.
    func delete() async -> Result<Void, Error> {
        await io {
            await mutex.withLock {
                wallet = nil
                told = nil
                let dir = walletDir
                await off { try? FileManager.default.removeItem(atPath: dir) }
                AppLog.info(line: "wallet deleted")
            }
            // Nothing of it under way any more: a new wallet starts afresh.
            prefs.watched = nil
            prefs.warned = []
            prefs.askedAt = nil
            prefs.caughtUp = false
            seen = nil
            looking = false
            WatchJob.cancel()
            ScanJob.cancel()
            // The folder's backups stay hers. None of a wallet gone.
            stopBackups()
            // Its network may differ from the one chosen next.
            await lightLock.withLock { await startLight() }
            update { $0 = Ui(loaded: true, network: prefs.network, developer: prefs.developer, caughtUp: false) }
            await refresh()
        }
    }

    /// A call reaching the network or changing what the wallet holds.
    private func locked<T>(call: @escaping @Sendable (Wallet) throws -> T) async throws -> T {
        try await mutex.withLock { try await local(call: call) }
    }

    /// A call reading what the wallet holds, alongside any other.
    private func local<T>(call: @escaping @Sendable (Wallet) throws -> T) async throws -> T {
        guard let w = wallet else { throw LicashError.Invalid(reason: "there is no wallet yet") }
        return try await off { try call(w) }
    }

    private func io<T>(call: () async throws -> T) async -> Result<T, Error> {
        do {
            return .success(try await call())
        } catch {
            return .failure(error)
        }
    }

    /// `call` off the main thread, on a thread that may wait: the core and
    /// the light client block, for minutes at times.
    private nonisolated func off<T>(call: @escaping @Sendable () throws -> T) async throws -> T {
        try await withCheckedThrowingContinuation { done in
            DispatchQueue.global(qos: .userInitiated).async { done.resume(with: Result { try call() }) }
        }
    }

    private nonisolated func off<T>(call: @escaping @Sendable () -> T) async -> T {
        await withCheckedContinuation { done in
            DispatchQueue.global(qos: .userInitiated).async { done.resume(returning: call()) }
        }
    }

    /// Milliseconds on a clock that keeps counting while the phone sleeps.
    private nonisolated static func elapsedRealtime() -> Int64 {
        var now = timespec()
        clock_gettime(CLOCK_MONOTONIC, &now)
        return Int64(now.tv_sec) * 1000 + Int64(now.tv_nsec) / 1_000_000
    }

    /// Milliseconds since the epoch.
    private nonisolated static func currentTimeMillis() -> Int64 { Int64(Date().timeIntervalSince1970 * 1000) }

    /// Nanoseconds on a clock that only goes forward.
    private nonisolated static func nanoTime() -> Int64 { Int64(DispatchTime.now().uptimeNanoseconds) }

    /// A lock held across suspensions, those waiting let in in turn: the
    /// sections it guards run one at a time.
    @MainActor
    private final class Mutex {
        private var held = false
        private var waiting: [CheckedContinuation<Void, Never>] = []

        func lock() async {
            if !held {
                held = true
                return
            }
            await withCheckedContinuation { waiting.append($0) }
        }

        func tryLock() -> Bool {
            if held { return false }
            held = true
            return true
        }

        func unlock() {
            if waiting.isEmpty { held = false } else { waiting.removeFirst().resume() }
        }

        func withLock<T>(action: () async throws -> T) async rethrows -> T {
            await lock()
            defer { unlock() }
            return try await action()
        }
    }

    private static let ROUND_MS: Int64 = 20_000
    private static let OPENING_TICK_MS: Int64 = 2_000
    private static let EAGER_MS: Int64 = 4_000
    private static let MAIL_MS: Int64 = 1_000

    /// How long a wait for pushed mail lasts at most: one on a wallet since replaced ends by then.
    private static let PUSH_WAIT_S: UInt32 = 20
    private static let AWAY_MS: Int64 = 60_000
    private static let CATCH_UP_AFTER_MS: Int64 = 15_000
    private static let BACKUP_REST_MS: Int64 = 30_000
    private static let BACKUP_MOST_MS: Int64 = 5 * 60_000
    private static let BACKUP_LOOK_MS: Int64 = 3600_000
    private static let BACKUP_RETRY_MS: Int64 = 5 * 60_000
    private static let PROVIDERS_MS: Int64 = 15 * 60_000
    private static let PROVIDERS_RETRY_MS: Int64 = 2 * 60_000

    /// Under the Tor client's 5 min keep of an idle connection, so that one asked ahead stays open.
    private static let WARM_MS: Int64 = 4 * 60_000
    private static let OFFER_MS: Int64 = 30 * 60_000
    private static let HUBS_MS: Int64 = 5 * 60_000
    private static let QUOTE_MS: Int64 = 4 * 60_000
}

/// The coin `q` is offered for, by its outpoint: its largest.
func offeredCoin(q: ContractQuote) -> String? { q.coins.first?.outpoint }

/// Whether the offer `q` was declined: its coin's, `declined` the coin last declined.
func offerDeclined(q: ContractQuote?, declined: String?) -> Bool {
    guard let q, let declined else { return false }
    return offeredCoin(q: q) == declined
}
