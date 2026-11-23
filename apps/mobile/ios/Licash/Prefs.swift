import Foundation
import LicashCore

/// What the app keeps besides the wallet: settings, mostly for developers.
final class Prefs {
    private let prefs = UserDefaults.standard

    /// Shows the test networks and the light client's settings.
    var developer: Bool {
        get { prefs.bool(forKey: "developer") }
        set { prefs.set(newValue, forKey: "developer") }
    }

    /// The network a new wallet is made for.
    var network: Network {
        get { [Network.bitcoin, .signet, .mutinynet, .regtest].first { "\($0)".uppercased() == prefs.string(forKey: "network") } ?? .bitcoin }
        set { prefs.set("\(newValue)".uppercased(), forKey: "network") }
    }

    /// The only peers of the light client, comma separated, or none.
    var peers: String {
        get { prefs.string(forKey: "peers") ?? "" }
        set { prefs.set(newValue.trimmingCharacters(in: .whitespacesAndNewlines), forKey: "peers") }
    }

    /// The block-dn server to sync from: "default", a URL, or empty.
    var cdn: String {
        get { prefs.string(forKey: "cdn") ?? "default" }
        set { prefs.set(newValue.trimmingCharacters(in: .whitespacesAndNewlines), forKey: "cdn") }
    }

    /// The block explorer transactions link to: "default", a URL, or empty for none.
    var explorer: String {
        get { prefs.string(forKey: "explorer") ?? "default" }
        set { prefs.set(newValue.trimmingCharacters(in: .whitespacesAndNewlines), forKey: "explorer") }
    }

    /// Hubs and relays reached through Tor: by default but on regtest, whose
    /// services are local. [network] is that of the wallet, chosen once.
    var tor: Bool {
        get { prefs.object(forKey: "tor") as? Bool ?? (network != .regtest) }
        set { prefs.set(newValue, forKey: "tor") }
    }

    /// The coin on Bitcoin (`txid:vout`) whose offer of protection was last declined, if any.
    var offerDeclined: String? {
        get { prefs.string(forKey: "offer_declined") }
        set { prefs.set(newValue, forKey: "offer_declined") }
    }

    /// How much was not protected at `hub` when the owner last dismissed the
    /// suggestion to protect it or cash part out: suggested again only once
    /// more is not protected.
    func unprotectedDismissed(hub: String) -> UInt64? {
        guard let amount = prefs.object(forKey: dismissedKey(hub: hub)) as? Int64, amount >= 0 else { return nil }
        return UInt64(amount)
    }

    func dismissUnprotected(hub: String, amount: UInt64) {
        prefs.set(Int64(bitPattern: amount), forKey: dismissedKey(hub: hub))
    }

    private func dismissedKey(hub: String) -> String {
        "unprotected_dismissed:" + String(hub.reversed().drop { $0 == "/" }.reversed())
    }

    /// The name the hub at `url` announced itself under when it was added from those found, if it was.
    func hubName(url: String) -> String? { prefs.string(forKey: hubNameKey(hub: url)) }

    func nameHub(url: String, name: String) { prefs.set(name, forKey: hubNameKey(hub: url)) }

    private func hubNameKey(hub: String) -> String { "hub_name:" + String(hub.reversed().drop { $0 == "/" }.reversed()) }

    /// Whether [tor] was set rather than the default.
    var torChosen: Bool { prefs.object(forKey: "tor") != nil }

    /// The most paid to renew notes a hub that restarted no longer unlinks,
    /// in parts per million of them: none if never set, the wallet's own.
    var renewPpm: UInt32? {
        get {
            guard let ppm = prefs.object(forKey: "renew_ppm") as? Int64, ppm >= 0 else { return nil }
            return UInt32(truncatingIfNeeded: ppm)
        }
        set { prefs.set(newValue.map(Int64.init) ?? -1, forKey: "renew_ppm") }
    }

    /// Local planning only. A missing preference never authorizes a close.
    /// Stored under its name as the proof margin, so upgrades keep it.
    var exitMarginDays: Int {
        get {
            let days = prefs.object(forKey: "proof_margin_days") as? Int ?? 14
            return (1...30).contains(days) ? days : 14
        }
        set {
            precondition((1...30).contains(newValue))
            prefs.set(newValue, forKey: "proof_margin_days")
        }
    }

    /// The lines of the history under way when last looked, by key: none
    /// kept yet (`nil`, as before the app kept them) tells of none finished.
    var watched: Set<String>? {
        get { (prefs.array(forKey: "watched") as? [String]).map(Set.init) }
        set { if let keys = newValue { prefs.set(Array(keys), forKey: "watched") } else { prefs.removeObject(forKey: "watched") } }
    }

    /// Whether the wallet looked for its payments up to the chain's tip once: its first catching up over.
    var caughtUp: Bool {
        get { prefs.bool(forKey: "caught_up") }
        set { prefs.set(newValue, forKey: "caught_up") }
    }

    /// The exits and disputes already told of, by [ExitNotice.key].
    var exitsTold: Set<String> {
        get { Set(prefs.array(forKey: "exits_told") as? [String] ?? []) }
        set { prefs.set(Array(newValue), forKey: "exits_told") }
    }

    /// The payouts already warned of as their deadline neared, by key.
    var warned: Set<String> {
        get { Set(prefs.array(forKey: "warned") as? [String] ?? []) }
        set { prefs.set(Array(newValue), forKey: "warned") }
    }

    /// When a Lightning invoice was last asked, in seconds since the epoch, if ever since kept.
    var askedAt: Int64? {
        get {
            guard let at = prefs.object(forKey: "asked_at") as? Int64, at >= 0 else { return nil }
            return at
        }
        set { prefs.set(newValue ?? -1, forKey: "asked_at") }
    }

    /// The folder (a bookmark of it) automatic backups go to: none if off.
    var backupFolder: String? {
        get { prefs.string(forKey: "backup_folder") }
        set { prefs.set(newValue, forKey: "backup_folder") }
    }

    /// When an automatic backup was last written whole, in ms since the epoch, if ever.
    var backupAt: Int64? {
        get {
            guard let at = prefs.object(forKey: "backup_at") as? Int64, at >= 0 else { return nil }
            return at
        }
        set { prefs.set(newValue ?? -1, forKey: "backup_at") }
    }
}
