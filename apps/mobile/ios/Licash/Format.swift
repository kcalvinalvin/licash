import Foundation
import LicashCore

/// `12,345 sat`, commas between thousands.
func sat(amount: UInt64) -> String { "\(digits(amount: amount)) sat" }

/// `12,345`.
func digits(amount: UInt64) -> String { Int64(bitPattern: amount).formatted(.number.locale(Locale(identifier: "en_US"))) }

extension Balance {
    /// All the wallet holds: in notes, and on Bitcoin.
    var total: UInt64 { notes + notesPending + onchain + moving + opening }

    /// Of [total], what hubs owe on trust alone: the notes no contract covers.
    /// The rest, on Bitcoin or covered by a contract, is the wallet's own.
    var credit: UInt64 {
        let held = notes + notesPending
        return held > covered ? held - covered : 0
    }

    /// [credit] as a share of [total], in whole percent: 0 if nothing is held.
    /// Never 0 with some on credit, nor 100 with some protected.
    var creditPercent: Int {
        if total == 0 { return 0 }
        let rounded = Int(Double(credit) * 100 / Double(total) + 0.5)
        if credit > 0 && rounded == 0 { return 1 }
        if credit < total && rounded == 100 { return 99 }
        return rounded
    }
}

/// What [fee] is of [of], to two significant digits: `0.24%`, `1.2%`, `5%`,
/// `12%`, `<0.01%` below. Nil for no fee, or a fee of nothing.
func share(fee: UInt64, of: UInt64) -> String? {
    if fee == 0 || of == 0 { return nil }
    // Halves rounded up, as far as Int64 holds it.
    let at = { (scale: Int64) -> Int64 in
        let x = (Double(fee) * Double(scale) / Double(of)).rounded(.toNearestOrAwayFromZero)
        return x < Double(Int64.max) ? Int64(x) : Int64.max
    }
    let hundredths = at(10_000)
    if hundredths == 0 { return "<0.01%" }
    if hundredths < 100 { return String(format: "0.%02lld%%", locale: Locale(identifier: "en_US"), hundredths) }
    let tenths = at(1_000)
    if tenths < 100 && tenths % 10 != 0 { return "\(tenths / 10).\(tenths % 10)%" }
    if tenths < 100 { return "\(tenths / 10)%" }
    return "\(digits(amount: UInt64(at(100))))%"
}

/// A fee and what it is of [of]: `12 sat · 0.24%`.
func feeOf(fee: UInt64, of: UInt64) -> String { sat(amount: fee) + (share(fee: fee, of: of).map { " · \($0)" } ?? "") }

/// `ppm` millionths as a percentage: `0.5%`.
func percent(ppm: UInt64) -> String {
    let style = FloatingPointFormatStyle<Double>(locale: Locale(identifier: "en_US"))
        .grouping(.never).precision(.fractionLength(0...4)).rounded(rule: .toNearestOrEven)
    return style.format(Double(ppm) / 10_000) + "%"
}

/// What went wrong, for the user.
func explain(e: Error) -> String {
    switch e {
    case LicashError.Invalid(let reason): return plain(reason: reason)
    case LicashError.Unavailable(let reason): return plain(reason: reason)
    case LicashError.Pending: return "Still under way: it finishes by itself."
    case LicashError.Insufficient(let needed, let available):
        return "Not enough: \(sat(amount: needed)) needed, \(sat(amount: available)) available."
    case LicashError.NeedsTor: return "This hub is on Tor: turn Tor on in Settings."
    case LicashError.UnknownHub(let url): return "This payment is at a hub you don't use yet: \(url)"
    case LicashError.FeesRaised(let hub):
        return "\(host(url: hub)) raised its fees since your protection there started. Approve them in Settings, under Hubs, to pay there."
    case LicashError.Failed(let reason): return plain(reason: reason)
    case LicashError.NoteUnkept(let hub): return "\(host(url: hub)) can't carry a note with a payment."
    case ChainError.Failed(let reason): return plain(reason: reason)
    // Its message: its fields, each named.
    case LicashError.FeeRose(let who, let agreed, let asked, let reason):
        return plain(reason: "who=\(who), agreed=\(agreed), asked=\(asked), reason=\(reason)")
    default: return plain(reason: e.localizedDescription)
    }
}

/// A reason the core gives, in the app's words: a sentence, its amounts
/// grouped, and no one to pay out or move said without naming providers.
func plain(reason: String) -> String {
    func amount(m: Regex<(Substring, Substring)>.Match) -> String { UInt64(m.1).map { sat(amount: $0) } ?? "\(m.1) sat" }
    if let m = reason.wholeMatch(of: NO_PAYOUT) { return "No one can send \(amount(m: m)) on Bitcoin right now. Try less, or later." }
    if let m = reason.wholeMatch(of: NO_MOVE) { return "No one can move \(amount(m: m)) between your hubs right now. Try less, or later." }
    if let m = reason.wholeMatch(of: NO_HUB) { return "Your balance can't pay \(amount(m: m)) and its fee here." }
    let grouped = reason.replacing(AMOUNT) { amount(m: $0) }
    return grouped.prefix(1).uppercased() + grouped.dropFirst()
}

private let NO_PAYOUT = #/no swap provider found pays (\d+) sat out on Bitcoin now/#
private let NO_MOVE = #/no swap provider found moves (\d+) sat from \S+ to \S+ now/#

/// The core's reason when no hub a code names holds the amount and its fee, with where to pay on Bitcoin instead, if the code says.
private let NO_HUB = #/no hub it names is one trusted holding (\d+) and the fee(?:: pay \S+ on Bitcoin)?/#
private let AMOUNT = #/\b(\d{4,}) sat\b/#.wordBoundaryKind(.simple)

/// The host of a hub's `url`, to name it short: with its port if it names
/// one, as hubs sharing a host do.
func host(url: String) -> String {
    guard let uri = URLComponents(string: url), let host = uri.host else { return url }
    return uri.port.map { "\(host):\($0)" } ?? host
}

/// About how long `blocks` take, `blockSecs` each (ten minutes on Bitcoin's networks): `about 6 days`.
func duration(blocks: Int64, blockSecs: Int64 = 600) -> String {
    let minutes = blocks * blockSecs / 60
    if blocks <= 0 { return "now" }
    if minutes < 90 { return "about \(minutes) minutes" }
    if minutes < 48 * 60 { return "about \((minutes + 30) / 60) hours" }
    return "about \((minutes + 720) / 1440) days"
}

/// When, from seconds since 1970.
func at(seconds: Int64) -> String {
    let format = DateFormatter()
    format.dateStyle = .medium
    format.timeStyle = .short
    return format.string(from: Date(timeIntervalSince1970: TimeInterval(seconds)))
}

/// Whether `e` says no swap provider pays the amount out on Bitcoin now.
func noPayout(e: Error) -> Bool {
    if case LicashError.Invalid(let reason) = e { return reason.wholeMatch(of: NO_PAYOUT) != nil }
    return false
}

/// The most to offer instead of `asked`, none paying it out: `most`, if some and less.
func offeredInstead(asked: UInt64?, most: UInt64?) -> UInt64? {
    guard let most, let asked, most >= 1 && most < asked else { return nil }
    return most
}

/// Whether `e` says no hub the code names holds the amount and its fee: the payer holds too little there.
func noHub(e: Error) -> Bool {
    if case LicashError.Invalid(let reason) = e { return reason.wholeMatch(of: NO_HUB) != nil }
    return false
}
