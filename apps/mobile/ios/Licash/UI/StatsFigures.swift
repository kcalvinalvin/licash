import Foundation
import LicashCore

/// The core's `Flow`, named apart from the app's.
private typealias Purpose = LicashCore.Flow

// What the Stats screen says, worked out from the core's events: the
// period asked for, what came and went in it, what it cost and how long it
// took. Nothing here draws, and nothing is stored: the same events and the
// same period give the same figures.

/// How much of the past a page of Stats covers.
enum Period: String, CaseIterable {
    case day = "Day"
    case week = "Week"
    case month = "Month"
    case year = "Year"
    case all = "All"

    var word: String { rawValue }
}

/// A stretch of time in seconds since 1970: from `start`, up to but not with `end`.
struct Span: Equatable {
    var start: Int64
    var end: Int64

    func contains(at: Int64) -> Bool { at >= start && at < end }
}

/// The whole of time: what All covers.
let EVER = Span(start: Int64.min, end: Int64.max)

/// The span `back` periods before the one `now` is in, its days by `zone`'s
/// clock and weeks from `firstDay`. All has none before it.
func span(
    period: Period,
    now: Int64,
    back: Int,
    zone: TimeZone = TimeZone.current,
    firstDay: Locale.Weekday = .monday
) -> Span {
    if period == .all { return EVER }
    // Days by the Gregorian calendar, whatever calendar the phone shows.
    var calendar = Calendar(identifier: .gregorian)
    calendar.timeZone = zone
    let today = calendar.startOfDay(for: Date(timeIntervalSince1970: TimeInterval(now)))
    let plus = { (unit: Calendar.Component, count: Int, day: Date) in
        calendar.date(byAdding: unit, value: count, to: day)!
    }
    let from: Date
    let to: Date
    switch period {
    case .day:
        from = plus(.day, -back, today)
        to = plus(.day, 1, from)
    case .week:
        // Back to the last `firstDay`, or today if it is one. The calendar numbers weekdays from Sunday, 1.
        let weekdays: [Locale.Weekday] = [.sunday, .monday, .tuesday, .wednesday, .thursday, .friday, .saturday]
        let first = weekdays.firstIndex(of: firstDay)! + 1
        let since = (calendar.component(.weekday, from: today) - first + 7) % 7
        from = plus(.weekOfYear, -back, plus(.day, -since, today))
        to = plus(.weekOfYear, 1, from)
    case .month:
        from = plus(.month, -back, calendar.date(from: calendar.dateComponents([.year, .month], from: today))!)
        to = plus(.month, 1, from)
    default:
        from = plus(.year, -back, calendar.date(from: calendar.dateComponents([.year], from: today))!)
        to = plus(.year, 1, from)
    }
    let at = { (day: Date) in Int64(calendar.startOfDay(for: day).timeIntervalSince1970) }
    return Span(start: at(from), end: at(to))
}

/// What a span of `period`, `back` periods ago, is called: `Today`, `Last week`, `October`, `2025`.
func label(
    period: Period,
    now: Int64,
    back: Int,
    zone: TimeZone = TimeZone.current,
    // The phone's language: `Locale.current` has the app's, which is only English.
    locale: Locale = Locale(identifier: Locale.preferredLanguages.first ?? "en"),
    firstDay: Locale.Weekday = .monday
) -> String {
    if period == .all { return "All time" }
    let span = span(period: period, now: now, back: back, zone: zone, firstDay: firstDay)
    var calendar = Calendar(identifier: .gregorian)
    calendar.timeZone = zone
    let first = Date(timeIntervalSince1970: TimeInterval(span.start))
    let last = Date(timeIntervalSince1970: TimeInterval(span.end - 1))
    let year = calendar.component(.year, from: Date(timeIntervalSince1970: TimeInterval(now)))
    let format = { (day: Date, pattern: String) -> String in
        let formatter = DateFormatter()
        // The digits 0 to 9, whatever the language.
        var latin = Locale.Components(locale: locale)
        latin.numberingSystem = Locale.NumberingSystem("latn")
        formatter.locale = Locale(components: latin)
        formatter.calendar = calendar
        formatter.timeZone = zone
        formatter.dateFormat = pattern
        return formatter.string(from: day)
    }
    return switch period {
    case .day:
        switch back {
        case 0: "Today"
        case 1: "Yesterday"
        default: format(first, calendar.component(.year, from: first) == year ? "MMM d" : "MMM d, yyyy")
        }
    case .week:
        switch back {
        case 0: "This week"
        case 1: "Last week"
        default:
            if calendar.component(.month, from: first) == calendar.component(.month, from: last) {
                format(first, "MMM d") + " – " + format(last, "d")
            } else {
                format(first, "MMM d") + " – " + format(last, "MMM d")
            }
        }
    case .month:
        switch back {
        case 0: "This month"
        case 1: "Last month"
        default: format(first, calendar.component(.year, from: first) == year ? "MMMM" : "MMMM yyyy")
        }
    default:
        switch back {
        case 0: "This year"
        case 1: "Last year"
        default: String(calendar.component(.year, from: first))
        }
    }
}

/// How many of a thing, and what they came to.
struct Tally: Equatable {
    var count: Int = 0
    var amount: UInt64 = 0

    static func + (tally: Tally, event: StatEvent) -> Tally {
        Tally(count: tally.count + 1, amount: tally.amount + event.amount)
    }
}

/// What a kind of thing cost, as its usual (median) and its average, over `count` done.
struct Costs: Equatable {
    var count: Int
    var usualFee: UInt64
    var meanFee: UInt64
    /// Fee over amount, in parts per million, for those with an amount.
    var usualPpm: Int64?
    var meanPpm: Int64?
    var usualSecs: Int64?
    var meanSecs: Int64?
}

/// What the settled fees paid in a span are, whom they paid and what for.
struct Fees: Equatable {
    var total: UInt64 = 0
    /// Sending, by how it was sent.
    var sending: [LicashCore.Way: UInt64] = [:]
    /// The ways of `sending`, in the order each first paid a fee.
    var ways: [LicashCore.Way] = []
    /// Bitcoin turned into notes, and notes moved between hubs.
    var moving: UInt64 = 0
    /// Optimizing, renewing and the notes made to pay exact amounts.
    var upkeep: UInt64 = 0
    var protection: UInt64 = 0
    /// Of the total, the hub's, swap providers' and Bitcoin's, as recorded.
    var hub: UInt64 = 0
    var provider: UInt64 = 0
    var network: UInt64 = 0
    /// The rest: a fee not told apart.
    var unsplit: UInt64 = 0
}

/// All of a span's figures.
struct Summary: Equatable {
    /// Two values together, as a tuple that can be compared.
    struct Pair<A: Equatable, B: Equatable>: Equatable {
        var first: A
        var second: B
    }

    var received: [LicashCore.Way: Tally]
    var sent: [LicashCore.Way: Tally]
    /// Payments that failed, and so cost and moved nothing.
    var failed: Int
    var fees: Fees
    /// What sending cost, by way.
    var sending: [LicashCore.Way: Costs]
    var moving: Costs?
    var swapping: Costs?
    var optimizing: Costs?
    var renewing: Costs?
    var normalizing: Costs?
    var protecting: Costs?
    /// What the fees were charged on: what moved, what was reshaped, what contracts protect at most.
    var moved: Tally
    var reshaped: Tally
    var covered: Tally
    /// How long notes waited to be unlinked, in seconds.
    var unlinking: Pair<Int, Pair<Int64, Int64>>?
    /// The events in the span at all.
    var events: Int
}

private let WAYS: [LicashCore.Way] = [.licash, .lightning, .bitcoin]

/// What keeps the notes in shape: optimizing, renewing, and making notes of the exact amounts a payment needs.
private let UPKEEP: Set<Purpose> = [.optimized, .renewed, .normalized]

/// The figures of `events` and `unlinks` within `span`.
func summarize(events: [StatEvent], unlinks: [Unlink], span: Span) -> Summary {
    let within = events.filter { e in e.at.map { span.contains(at: $0) } ?? (span == EVER) }
    let done = within.filter { $0.state == .done }
    func tally(flow: Purpose) -> [LicashCore.Way: Tally] {
        Dictionary(uniqueKeysWithValues: WAYS.map { way in
            (way, done.filter { $0.flow == flow && $0.way == way }.reduce(Tally()) { t, e in t + e })
        }).filter { $0.value.count > 0 }
    }
    let of = { (flow: Purpose, way: LicashCore.Way?) in
        costs(events: done.filter { $0.flow == flow && (way == nil || $0.way == way) })
    }
    let moves = done.filter { $0.flow == .moved }
    let waited = unlinks.filter { span.contains(at: $0.at) || span == EVER }.map { Int64(truncatingIfNeeded: $0.waited) }
    return Summary(
        received: tally(flow: .received),
        sent: tally(flow: .sent),
        failed: within.filter { $0.flow == .sent && $0.state == .failed }.count,
        fees: fees(events: within),
        sending: Dictionary(uniqueKeysWithValues: WAYS.compactMap { way in of(.sent, way).map { (way, $0) } }),
        moving: costs(events: moves.filter { $0.way == .bitcoin }),
        swapping: costs(events: moves.filter { $0.way == .licash }),
        optimizing: of(.optimized, nil),
        renewing: of(.renewed, nil),
        normalizing: of(.normalized, nil),
        protecting: of(.protection, nil),
        moved: done.filter { $0.flow == .moved }.reduce(Tally()) { t, e in t + e },
        reshaped: done.filter { UPKEEP.contains($0.flow) }.reduce(Tally()) { t, e in t + e },
        covered: done.filter { $0.flow == .protection }.reduce(Tally()) { t, e in t + e },
        unlinking: waited.isEmpty ? nil : Summary.Pair(
            first: waited.count,
            second: Summary.Pair(first: median(values: waited), second: waited.reduce(0, +) / Int64(waited.count))
        ),
        events: within.count
    )
}

/// The settled fees of `events`: the core already counts none for what failed or is under way.
func fees(events: [StatEvent]) -> Fees {
    var f = Fees()
    for e in events {
        if e.fee == 0 { continue }
        let told = (e.hubFee ?? 0) + (e.providerFee ?? 0) + (e.networkFee ?? 0)
        f.total += e.fee
        f.hub += e.hubFee ?? 0
        f.provider += e.providerFee ?? 0
        f.network += e.networkFee ?? 0
        f.unsplit += e.fee > told ? e.fee - told : 0
        switch e.flow {
        case .sent:
            if f.sending[e.way] == nil { f.ways.append(e.way) }
            f.sending[e.way] = (f.sending[e.way] ?? 0) + e.fee
        case .moved: f.moving += e.fee
        case let flow where UPKEEP.contains(flow): f.upkeep += e.fee
        case .protection: f.protection += e.fee
        default: break
        }
    }
    return f
}

/// What `events`, all done, cost: none if there are none.
func costs(events: [StatEvent]) -> Costs? {
    if events.isEmpty { return nil }
    let fees = events.map { $0.fee }
    let ratios = events.filter { $0.amount > 0 }.map { Double($0.fee) / Double($0.amount) * 1_000_000 }
    let times = events.compactMap { $0.took.map { Int64(truncatingIfNeeded: $0) } }
    return Costs(
        count: events.count,
        usualFee: UInt64(truncatingIfNeeded: median(values: fees.map { Int64(truncatingIfNeeded: $0) })),
        meanFee: UInt64(fees.reduce(0.0) { $0 + Double($1) } / Double(fees.count)),
        // A ratio is at most the largest Int64, for a fee far beyond its amount.
        usualPpm: ratios.isEmpty ? nil : Int64(exactly: medianOf(values: ratios).rounded(.towardZero)) ?? .max,
        meanPpm: ratios.isEmpty ? nil : Int64(exactly: (ratios.reduce(0, +) / Double(ratios.count)).rounded(.towardZero)) ?? .max,
        usualSecs: times.isEmpty ? nil : median(values: times),
        meanSecs: times.isEmpty ? nil : times.reduce(0, +) / Int64(times.count)
    )
}

/// The middle of `values`, the mean of the two if there are an even number.
func median(values: [Int64]) -> Int64 {
    let sorted = values.sorted()
    let mid = sorted.count / 2
    return sorted.count % 2 == 1 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2
}

private func medianOf(values: [Double]) -> Double {
    let sorted = values.sorted()
    let mid = sorted.count / 2
    return sorted.count % 2 == 1 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2
}

/// A fee's share as a ratio in parts per million: `0.24%`, or nil for none.
func ppmShare(ppm: Int64?) -> String? {
    ppm.flatMap { $0 > 0 ? share(fee: UInt64($0), of: 1_000_000) : nil }
}

/// A fee and its share: `12 sat · 0.24%`, `None` for nothing.
func cost(fee: UInt64, ppm: Int64?) -> String {
    fee == 0 ? "None" : sat(amount: fee) + (ppmShare(ppm: ppm).map { " · \($0)" } ?? "")
}

/// A span of seconds as short as it can be said: `4 s`, `2 min`, `3 h`, `2 days`.
func took(seconds: Int64) -> String {
    if seconds < 1 { return "under a second" }
    if seconds < 90 { return "\(seconds) s" }
    if seconds < 90 * 60 { return "\((seconds + 30) / 60) min" }
    if seconds < 48 * 3600 { return "\((seconds + 1800) / 3600) h" }
    return "\((seconds + 43_200) / 86_400) days"
}

/// `3 · 12,345 sat`: how many, and what they came to.
func tallied(t: Tally) -> String { "\(digits(amount: UInt64(t.count))) · \(sat(amount: t.amount))" }

/// The instants to read the balance at across `span`, up to `now`: the
/// start, then `steps` more spread evenly to the end. A span that began
/// before the first thing that happened, `first`, starts there. So does All.
func instants(span: Span, now: Int64, first: Int64?, steps: Int = 24) -> [Int64] {
    let end = min(span.end, now)
    // From the first thing that happened if that is after the span began: before it, nothing was held.
    let start: Int64 = if span == EVER {
        first ?? end
    } else if let first, first > span.start {
        first - 1
    } else {
        span.start
    }
    if end <= start { return [end] }
    var seen = Set<Int64>()
    return (0...steps).map { start + (end - start) * Int64($0) / Int64(steps) }.filter { seen.insert($0).inserted }
}

/// What the wallet held at the start and the end of a span, and how it changed, in sat.
struct Change: Equatable {
    var start: UInt64
    var end: UInt64

    var up: Bool { end >= start }
    var by: UInt64 { end >= start ? end - start : start - end }
}

extension BalancePoint {
    var total: UInt64 { licash + bitcoin }
}

/// The change over `points`, none if there is no point.
func change(points: [BalancePoint]) -> Change? {
    points.isEmpty ? nil : Change(start: points[0].total, end: points[points.count - 1].total)
}

// How much of the balance was protected on average. Coverage is only known
// live, so the wallet writes its balance down as it changes. This reads
// those samples back. A sample holds until the next, as the wallet's balance
// moves only while the app runs, but not across a gap: two samples more than
// `HOLDS` apart that differ say nothing of the time between, which is left
// out and said.

/// The longest a changed balance is taken to have held with no sample after it: a day and a half.
let HOLDS: Int64 = 36 * 3600

/// What the wallet held, protected and not, on average over what was recorded of a span.
struct Averaged: Equatable {
    var protected: UInt64
    var unprotected: UInt64
    /// Protected over all held, over the recorded time, in parts per million.
    var ppm: Int64
    /// Seconds of the span that were recorded, of `spanSecs`.
    var recorded: Int64
    var spanSecs: Int64
    /// When the first sample was written: nothing is known of before it.
    var since: Int64
}

/// The average of `samples`, oldest first, over `span` up to `now`: none if
/// none of it was recorded. Absolute figures are means over recorded time
/// and the share is all protected over all held, not the mean of shares.
func averaged(samples: [BalanceSample], span: Span, now: Int64) -> Averaged? {
    if samples.isEmpty { return nil }
    let first = samples[0].at
    let from = max(span.start, first)
    let to = min(span.end, now)
    if to <= from { return nil }
    var recorded: Int64 = 0
    var protectedSecs = 0.0
    var unprotectedSecs = 0.0
    for (i, a) in samples.enumerated() {
        let next = i + 1 < samples.count ? samples[i + 1] : nil
        // Until the next sample if it is near or says the same. The last, until now if recent.
        let until: Int64? = if let next, next.at - a.at <= HOLDS || same(a: a.balance, b: next.balance) {
            next.at
        } else if next == nil && now - a.at <= HOLDS {
            now
        } else {
            nil
        }
        guard let until else { continue }
        let begin = max(a.at, from)
        let end = min(until, to)
        if end <= begin { continue }
        let secs = Double(end - begin)
        let p = protection(balance: a.balance)
        recorded += end - begin
        protectedSecs += Double(p.protected) * secs
        unprotectedSecs += Double(p.open) * secs
    }
    if recorded == 0 { return nil }
    let all = protectedSecs + unprotectedSecs
    return Averaged(
        protected: UInt64(protectedSecs / Double(recorded)),
        unprotected: UInt64(unprotectedSecs / Double(recorded)),
        ppm: all == 0 ? 0 : Int64(protectedSecs / all * 1_000_000),
        recorded: recorded,
        // Of the span so far, unless it has no beginning: then, all there is.
        spanSecs: to - (span.start == Int64.min ? from : span.start),
        since: first
    )
}

private func same(a: Balance, b: Balance) -> Bool { a == b }
