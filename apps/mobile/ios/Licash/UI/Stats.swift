import SwiftUI
import LicashCore

/// Stats: what came in and went out in a day, week, month, year or all of
/// it, what it cost and how long it took, so that what is cheap and what is
/// dear shows. Read from the wallet's own records when opened and as its
/// history changes. The balance is replayed from them.
struct StatsScreen: View {
    let engine: Engine
    let ui: Ui
    let back: () -> Void

    @SwiftUI.State private var now = Int64(Date().timeIntervalSince1970)
    @SwiftUI.State private var period = Period.month
    @SwiftUI.State private var ago = 0
    @SwiftUI.State private var stats: Stats? = nil
    @SwiftUI.State private var error: String? = nil
    @SwiftUI.State private var points: [BalancePoint] = []
    // None written down yet is `nil`. One that cannot be read, `failed`.
    @SwiftUI.State private var samples: [BalanceSample]? = nil
    @SwiftUI.State private var unread = false
    @SwiftUI.State private var firstDay = Locale.current.firstDayOfWeek

    init(engine: Engine, ui: Ui, back: @escaping () -> Void) {
        self.engine = engine
        self.ui = ui
        self.back = back
    }

    private var first: Int64? { stats?.events.compactMap { $0.at }.min() }
    private var shown: Span { span(period: period, now: now, back: ago, firstDay: firstDay) }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                TopBar(title: "Stats", back: back) {
                    Info(
                        title: "Stats",
                        text: "Worked out on your phone from your own payments. Fees count once paid; a payment that failed costs " +
                            "nothing. Usual is the middle of them, so one odd payment does not move it; the average is in the details."
                    )
                }
                Segmented(options: Period.allCases.map { $0.word }, chosen: Period.allCases.firstIndex(of: period)!) { i in
                    period = Period.allCases[i]
                    ago = 0
                }
                .frame(maxWidth: .infinity)
                if period != .all {
                    Stepper(
                        label: label(period: period, now: now, back: ago, firstDay: firstDay),
                        older: first.map { shown.start > $0 } ?? false,
                        newer: ago > 0,
                        back: { ago += 1 },
                        forward: { ago -= 1 }
                    )
                }
                if let error {
                    Text(error)
                        .textStyle(Theme.type.bodySmall)
                        .foregroundStyle(Theme.color.error)
                        .padding(4)
                        .accessibilityIdentifier("error")
                }
                if let data = stats {
                    let summary = summarize(events: data.events, unlinks: data.unlinks, span: shown)
                    if summary.events == 0 && points.allSatisfy({ $0.total == 0 }) {
                        Quiet(text: period == .all ? "Nothing yet" : "Nothing this \(period.word.lowercased())")
                    } else {
                        Overview(s: summary)
                        Balance(points: points, developer: ui.developer)
                        Protected(samples: samples, unread: unread, shown: shown, now: now, period: period)
                        Flows(s: summary)
                        Costing(s: summary)
                        Spending(s: summary, developer: ui.developer)
                        Timing(s: summary, developer: ui.developer)
                        Spacer().frame(height: 32)
                    }
                } else if error == nil {
                    Quiet(text: "Reading your payments")
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 20)
        }
        .accessibilityIdentifier("stats")
        .task(id: ui.history) {
            let read = await engine.stats()
            // Replaced by a newer run, this one's answer is stale.
            if Task.isCancelled { return }
            switch read {
            case .success(let it):
                stats = it
                error = nil
            case .failure(let e):
                error = explain(e: e)
            }
        }
        .task(id: ui.balance) {
            let read = await engine.balanceSamples()
            if Task.isCancelled { return }
            switch read {
            case .success(let it):
                samples = it
                unread = false
            case .failure:
                unread = true
            }
        }
        .task(id: [AnyHashable(period), AnyHashable(ago), AnyHashable(first), AnyHashable(ui.history)]) {
            let read = await engine.balanceOver(instants: instants(span: shown, now: now, first: first))
            if Task.isCancelled { return }
            if case .success(let it) = read {
                points = it
            }
        }
    }
}

/// The word of a way, as the app says it everywhere.
private extension LicashCore.Way {
    func said() -> String {
        switch self {
        case .licash: "In Licash"
        case .lightning: "Over Lightning"
        case .bitcoin: "On Bitcoin"
        }
    }
}

/// The entries of `map` in the order the app lists ways: in Licash, over Lightning, on Bitcoin.
private func listed<T>(map: [LicashCore.Way: T]) -> [(key: LicashCore.Way, value: T)] {
    [LicashCore.Way.licash, .lightning, .bitcoin].compactMap { way in map[way].map { (key: way, value: $0) } }
}

private struct Quiet: View {
    let text: String

    var body: some View {
        Text(text)
            .textStyle(Theme.type.bodyLarge)
            .foregroundStyle(Theme.color.onSurfaceVariant)
            .padding(.horizontal, 4)
            .padding(.vertical, 24)
            .accessibilityIdentifier("stats-none")
    }
}

/// The period asked for, with a step to the one before and the one after.
private struct Stepper: View {
    let label: String
    let older: Bool
    let newer: Bool
    let back: () -> Void
    let forward: () -> Void

    var body: some View {
        HStack(alignment: .center, spacing: 0) {
            IconButton(onClick: back, enabled: older) {
                Image(systemName: "chevron.backward")
                    .accessibilityLabel("Earlier")
            }
            .accessibilityIdentifier("stats-older")
            Text(label)
                .textStyle(Theme.type.titleMedium)
                .multilineTextAlignment(.center)
                .frame(maxWidth: .infinity)
                .accessibilityIdentifier("stats-period")
            IconButton(onClick: forward, enabled: newer) {
                Image(systemName: "arrow.forward")
                    .accessibilityLabel("Later")
            }
            .accessibilityIdentifier("stats-newer")
        }
        .frame(maxWidth: .infinity)
        .padding(.top, 8)
    }
}

/// What the fees came to, first: the number that says whether it is cheap.
private struct Overview: View {
    let s: Summary

    var body: some View {
        let sent = s.sent.values.reduce(UInt64(0)) { sum, t in sum + t.amount }
        Spacer().frame(height: 16)
        Text("Fees paid")
            .textStyle(Theme.type.labelLarge)
            .foregroundStyle(Theme.color.onSurfaceVariant)
            .padding(.horizontal, 4)
        HStack(alignment: .bottom, spacing: 0) {
            Text(s.fees.total == 0 ? "None" : sat(amount: s.fees.total))
                .textStyle(Heading)
                .accessibilityIdentifier("stats-fees")
            if let it = share(fee: s.fees.total, of: sent) {
                Text(" · \(it) of what you sent")
                    .textStyle(Theme.type.bodyMedium)
                    .foregroundStyle(Theme.color.outline)
                    .padding(.bottom, 6)
                    .accessibilityIdentifier("stats-fees-share")
            }
        }
        .padding(.horizontal, 4)
    }
}

/// What the wallet held, drawn over the period, and its start, end and change.
private struct Balance: View {
    let points: [BalancePoint]
    let developer: Bool

    var body: some View {
        if let change = change(points: points) {
            Group(title: "Balance") {
                Chart(points: points)
                Line(label: "At the start", value: sat(amount: change.start), tag: "stats-start")
                Line(label: "At the end", value: sat(amount: change.end), tag: "stats-end")
                let sign = change.up ? "+" : "−"
                Line(
                    label: "Change",
                    value: change.by == 0 ? "None" : "\(sign)\(sat(amount: change.by))",
                    tag: "stats-change",
                    share: share(fee: change.by, of: change.start)
                )
                let last = points[points.count - 1]
                More(label: "Where it was") {
                    Line(label: "In Licash", value: sat(amount: last.licash))
                    if last.bitcoin > 0 { Line(label: "On Bitcoin", value: sat(amount: last.bitcoin)) }
                    // A hub is named by its host, as everywhere: in basic mode too, as that is its name.
                    ForEach(last.hubs, id: \.self) { it in Line(label: name(url: it.hub), value: sat(amount: it.amount)) }
                    Line(label: "Most", value: sat(amount: points.map { $0.total }.max()!))
                    Line(label: "Least", value: sat(amount: points.map { $0.total }.min()!))
                    if developer { Line(label: "Read at", value: digits(amount: UInt64(points.count)) + " moments") }
                }
            }
        }
    }
}

/// How much of what was held was protected on average, from what the wallet
/// wrote down as its balance changed: only from the first time this version
/// ran, and only for the time it was written down, which it says.
private struct Protected: View {
    let samples: [BalanceSample]?
    let unread: Bool
    let shown: Span
    let now: Int64
    let period: Period

    var body: some View {
        Group(title: "Protected") {
            let a = averaged(samples: samples ?? [], span: shown, now: now)
            if unread {
                Say(text: "Not available right now.")
            } else if samples?.isEmpty ?? true {
                Say(text: "Written down from the day this version first ran: nothing yet.")
            } else if a == nil {
                Say(text: "Nothing was written down \(period == .all ? "yet" : "in this " + period.word.lowercased()).")
            } else if let a {
                Line(label: "Protected, on average", value: sat(amount: a.protected), tag: "stats-protected", share: ppmShare(ppm: a.ppm))
                Line(
                    label: "Not protected, on average",
                    value: sat(amount: a.unprotected),
                    tag: "stats-unprotected",
                    share: ppmShare(ppm: 1_000_000 - a.ppm)
                )
                if a.recorded < a.spanSecs {
                    let pct = a.recorded * 100 / a.spanSecs
                    Line(label: "Written down for", value: pct == 0 ? "under 1% of it" : "\(pct)% of it", tag: "stats-recorded")
                }
                if a.since > shown.start { Line(label: "Since", value: at(seconds: a.since), tag: "stats-since") }
            }
        }
    }
}

/// What came and what went, how many and how much, by way.
private struct Flows: View {
    let s: Summary

    var body: some View {
        if !s.received.isEmpty {
            Group(title: "Received") {
                ForEach(listed(map: s.received), id: \.key) { way, t in
                    Line(label: way.said(), value: tallied(t: t), tag: "stats-received \(String(describing: way).uppercased())")
                }
                if s.received.count > 1 {
                    Line(
                        label: "All",
                        value: tallied(t: s.received.values.reduce(Tally()) { a, b in Tally(count: a.count + b.count, amount: a.amount + b.amount) })
                    )
                }
            }
        }
        if !s.sent.isEmpty || s.failed > 0 {
            Group(title: "Sent") {
                ForEach(listed(map: s.sent), id: \.key) { way, t in
                    Line(label: way.said(), value: tallied(t: t), tag: "stats-sent \(String(describing: way).uppercased())")
                }
                if s.sent.count > 1 {
                    Line(
                        label: "All",
                        value: tallied(t: s.sent.values.reduce(Tally()) { a, b in Tally(count: a.count + b.count, amount: a.amount + b.amount) })
                    )
                }
                if s.failed > 0 { Line(label: "Couldn't pay", value: digits(amount: UInt64(s.failed)), tag: "stats-failed") }
            }
        }
    }
}

/// Each way of paying, the cheapest first: what it usually cost and how
/// long it usually took, its average a tap away.
private struct Costing: View {
    let s: Summary

    var body: some View {
        let ranked = listed(map: s.sending).sorted { ($0.value.usualPpm ?? Int64.max) < ($1.value.usualPpm ?? Int64.max) }
        let others: [(String, Costs, Tally)] = [
            s.moving.map { ("From Bitcoin to notes", $0, s.moved) },
            s.swapping.map { ("Between your hubs", $0, s.moved) },
            s.protecting.map { ("Protection", $0, s.covered) }
        ].compactMap { $0 }
        if !ranked.isEmpty || !others.isEmpty {
            Group(title: "What it costs") {
                let worst = ranked.map { $0.value.usualPpm ?? 0 }.max() ?? 0
                ForEach(Array(ranked.enumerated()), id: \.element.key) { i, entry in
                    CostRow(
                        title: entry.key.said(),
                        c: entry.value,
                        cheapest: i == 0 && ranked.count > 1,
                        bar: bar(ppm: entry.value.usualPpm, worst: worst),
                        tag: "cost \(String(describing: entry.key).uppercased())"
                    )
                }
                if !ranked.isEmpty && !others.isEmpty { Divider() }
                ForEach(others, id: \.0) { title, c, _ in CostRow(title: title, c: c, cheapest: false, bar: nil, tag: "cost \(title)") }
            }
        }
    }
}

/// How long a fee's bar is against the dearest's: at least a sliver.
private func bar(ppm: Int64?, worst: Int64) -> Double? {
    guard let ppm, worst > 0 else { return nil }
    return min(max(Double(ppm) / Double(worst), 0.04), 1)
}

private struct CostRow: View {
    let title: String
    let c: Costs
    let cheapest: Bool
    let bar: Double?
    let tag: String

    @SwiftUI.State private var open = false

    init(title: String, c: Costs, cheapest: Bool, bar: Double?, tag: String) {
        self.title = title
        self.c = c
        self.cheapest = cheapest
        self.bar = bar
        self.tag = tag
    }

    var body: some View {
        let times = c.usualSecs.map { " · usually \(took(seconds: $0))" } ?? ""
        let sub = (cheapest ? "Cheapest · " : "") + "\(digits(amount: UInt64(c.count))) paid" + times
        VStack(alignment: .leading, spacing: 0) {
            Entry(title: title, value: cost(fee: c.usualFee, ppm: c.usualPpm), sub: sub, tag: tag, onClick: { open = true })
            if let bar {
                GeometryReader { box in
                    RoundedRectangle(cornerRadius: 2)
                        .fill(Theme.color.primary.opacity(0.55))
                        .frame(width: box.size.width * bar)
                }
                .frame(height: 4)
                Spacer().frame(height: 8)
            }
        }
        .sheet(isPresented: $open) {
            Sheet(title: title, dismiss: { open = false }) {
                Line(label: "Paid", value: digits(amount: UInt64(c.count)))
                Line(label: "Usual fee", value: cost(fee: c.usualFee, ppm: c.usualPpm), tag: "usual-fee")
                Line(label: "Average fee", value: cost(fee: c.meanFee, ppm: c.meanPpm), tag: "mean-fee")
                if let it = c.usualSecs { Line(label: "Usual time", value: took(seconds: it)) }
                if let it = c.meanSecs { Line(label: "Average time", value: took(seconds: it)) }
            }
        }
    }
}

/// What was paid, for what, and to whom.
private struct Spending: View {
    let s: Summary
    let developer: Bool

    var body: some View {
        let f = s.fees
        if f.total != 0 {
            Group(title: "Fees by purpose") {
                ForEach(f.ways.map { (key: $0, value: f.sending[$0]!) }, id: \.key) { way, fee in
                    let said = way.said()
                    Line(
                        label: "Sending \(said.prefix(1).lowercased() + said.dropFirst())",
                        value: sat(amount: fee),
                        share: share(fee: fee, of: s.sent[way]?.amount ?? 0)
                    )
                }
                if f.moving > 0 { Line(label: "Moving", value: sat(amount: f.moving), share: share(fee: f.moving, of: s.moved.amount)) }
                if f.upkeep > 0 {
                    Line(label: "Keeping notes in shape", value: sat(amount: f.upkeep), share: share(fee: f.upkeep, of: s.reshaped.amount))
                }
                if f.protection > 0 {
                    Line(label: "Protection", value: sat(amount: f.protection), share: share(fee: f.protection, of: s.covered.amount))
                }
                More(label: "Who was paid") {
                    Line(label: "Hubs", value: sat(amount: f.hub), tag: "paid-hubs")
                    if f.provider > 0 { Line(label: "Swap providers", value: sat(amount: f.provider)) }
                    if f.network > 0 { Line(label: "Bitcoin's miners", value: sat(amount: f.network)) }
                    if f.unsplit > 0 { Line(label: "Not itemized", value: sat(amount: f.unsplit)) }
                }
                if developer {
                    let parts: [(String, Costs)] = [
                        s.optimizing.map { ("Optimizing", $0) },
                        s.renewing.map { ("Renewing", $0) },
                        s.normalizing.map { ("Exact notes", $0) }
                    ].compactMap { $0 }
                    if !parts.isEmpty {
                        More(label: "Note upkeep") {
                            ForEach(parts, id: \.0) { title, c in
                                Line(
                                    label: title,
                                    value: "\(digits(amount: UInt64(c.count))) · usually \(sat(amount: c.usualFee))",
                                    tag: "upkeep \(title)",
                                    share: ppmShare(ppm: c.usualPpm)
                                )
                            }
                        }
                    }
                }
            }
        }
    }
}

/// How long things usually took.
private struct Timing: View {
    let s: Summary
    let developer: Bool

    private var rows: [(String, Int64)] {
        var rows: [(String, Int64)] = []
        for (way, c) in listed(map: s.sending) {
            let said = way.said()
            if let secs = c.usualSecs { rows.append(("Sending \(said.prefix(1).lowercased() + said.dropFirst())", secs)) }
        }
        if let secs = s.swapping?.usualSecs { rows.append(("Moving between hubs", secs)) }
        if let secs = s.optimizing?.usualSecs { rows.append(("Optimizing notes", secs)) }
        if developer, let secs = s.renewing?.usualSecs { rows.append(("Renewing notes", secs)) }
        if let unlinking = s.unlinking { rows.append(("Until notes are unlinked", unlinking.second.first)) }
        return rows
    }

    var body: some View {
        if !rows.isEmpty {
            Group(title: "How long it takes") {
                ForEach(rows, id: \.0) { title, secs in Line(label: title, value: took(seconds: secs), tag: "took \(title)") }
                More(label: "About these times") {
                    Say(text: "The middle of them, from the start of each to its end. A payment that is not done yet, or failed, is not timed.")
                    if let unlinking = s.unlinking {
                        Line(label: "Notes unlinked", value: digits(amount: UInt64(unlinking.first)))
                        Line(label: "Average wait", value: took(seconds: unlinking.second.second))
                    }
                }
            }
        }
    }
}

/// The balance over time: a line and the area under it, no more than the numbers say.
private struct Chart: View {
    let points: [BalancePoint]

    var body: some View {
        let line = Theme.color.primary
        let said = points.first.map { first in
            "Balance from \(sat(amount: first.total)) to \(sat(amount: points[points.count - 1].total))"
        } ?? "No balance"
        Canvas { context, size in
            if points.count < 2 { return }
            let values = points.map { Double($0.total) }
            let lo = values.min()!
            let hi = values.max()!
            let pad: CGFloat = 4
            let w = size.width
            let h = size.height - 2 * pad
            // A flat balance is a flat line in the middle.
            func y(v: Double) -> CGFloat { hi == lo ? pad + h / 2 : pad + h - CGFloat((v - lo) / (hi - lo)) * h }
            func x(i: Int) -> CGFloat { w * CGFloat(i) / CGFloat(points.count - 1) }
            var path = Path()
            path.move(to: CGPoint(x: x(i: 0), y: y(v: values[0])))
            for i in 1..<values.count { path.addLine(to: CGPoint(x: x(i: i), y: y(v: values[i]))) }
            var area = path
            area.addLine(to: CGPoint(x: w, y: size.height))
            area.addLine(to: CGPoint(x: 0, y: size.height))
            area.closeSubpath()
            context.fill(area, with: .color(line.opacity(0.14)))
            context.stroke(path, with: .color(line), style: StrokeStyle(lineWidth: 2, lineCap: .round))
        }
        .padding(.vertical, 12)
        .frame(maxWidth: .infinity)
        .frame(height: 120)
        .accessibilityElement()
        .accessibilityLabel(said)
        .accessibilityIdentifier("stats-chart")
    }
}

/// A bar chart, from Material's path data (Apache 2.0): what opens Stats.
let StatsGlyph = Image(size: CGSize(width: 24, height: 24)) { context in
    let bars = Path { p in
        p.addRect(CGRect(x: 5, y: 9.2, width: 3, height: 9.8))
        p.addRect(CGRect(x: 10.6, y: 5, width: 2.8, height: 14))
        p.addRect(CGRect(x: 16.2, y: 13, width: 2.8, height: 6))
    }
    context.fill(bars, with: .foreground)
}
