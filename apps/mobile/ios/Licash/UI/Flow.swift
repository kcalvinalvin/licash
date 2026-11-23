import LicashCore
import SwiftUI

/*
 * The notes a line of the history spent and made, drawn: each a bar as long
 * as its amount's bits (a note twice as large, a step longer), those spent on
 * the left, those made on the right by whom they went to.
 */

/// Notes made for one part of a payment: `to` whom, or the wallet's (`nil`, as received).
struct Bucket: Equatable {
    var to: PaidTo?
    var amount: UInt64
    var notes: [UInt64]
}

/// The notes spent, and those made, by whom they went to.
struct Flow: Equatable {
    var spent: [UInt64]
    var out: [Bucket]
}

/// The bits of `amount`: 1 for 1 sat, 13 for 4,096.
func bits(amount: UInt64) -> Int { 64 - amount.leadingZeroBitCount }

/// What `notes` spent and made, drawn: the parts paid, payee first. Or,
/// none paid (as received), the notes made the wallet's. Nothing to draw,
/// `nil`.
func flowOf(notes: NotesOf) -> Flow? {
    let spent = notes.spent.map(\.amount)
    let paid = notes.paid.filter { $0.amount > 0 }
        .sorted { ordinal(to: $0.to) < ordinal(to: $1.to) }
        .map { Bucket(to: $0.to, amount: $0.amount, notes: $0.notes) }
    let made = notes.made
    let out = !paid.isEmpty ? paid
        : made.isEmpty ? []
        : [Bucket(to: nil, amount: made.reduce(0) { $0 &+ $1.amount }, notes: made.map(\.amount))]
    return spent.isEmpty && out.isEmpty ? nil : Flow(spent: spent, out: out)
}

/// Where `to` stands among whom a payment pays, as the core lists them: payee first.
private func ordinal(to: PaidTo) -> Int {
    switch to {
    case .payee: 0
    case .itself: 1
    case .hub: 2
    case .kept: 3
    }
}

/// Whom a part of a payment of `kind` paid, in words.
func paidTo(kind: Kind, to: PaidTo?) -> String {
    switch to {
    case .payee:
        switch kind {
        case .lightning: "To the invoice, through the hub"
        case .payout, .swap: "To the provider"
        default: "To the payee"
        }
    case .itself: "To this wallet"
    case .hub: "The hub's fee"
    case .kept: "Kept"
    case nil: "Yours"
    }
}

/// Where notes of none spent came from, in words.
private func from(kind: Kind) -> String {
    switch kind {
    case .moved, .funded, .onchain: "From Bitcoin"
    case .invoice: "Over Lightning"
    case .swap: "From the provider"
    default: "From the payer"
    }
}

/// Whom a part went to, in a word: a heading of the drawing.
func shortTo(kind: Kind, to: PaidTo?) -> String {
    switch to {
    case .payee:
        switch kind {
        case .lightning: "Invoice"
        case .payout, .swap: "Provider"
        default: "Payee"
        }
    case .itself: "This wallet"
    case .hub: "Hub fee"
    case .kept: "Kept"
    case nil: "Yours"
    }
}

private let ROW: CGFloat = 22
private let BAR: CGFloat = 14
private let HEAD: CGFloat = 22
private let LABEL: CGFloat = 46

/// A bar's color by whom it went to. Spent, `nil`.
private func colors() -> (PaidTo?) -> Color {
    let scheme = Theme.color
    return { to in
        switch to {
        case .payee, .itself: scheme.primary
        case .hub: Color(red: 0xC9 / 255, green: 0x8A / 255, blue: 0x2B / 255)
        case .kept: scheme.secondary
        case nil: scheme.primary
        }
    }
}

/// The notes `activity` spent and made, drawn: no identifiers, only how
/// large each is and where it went. Under way, the notes not made yet show
/// faint, as one bar for their part.
struct NotesFlow: View {
    let activity: Activity
    let flow: Flow

    init(activity: Activity, flow: Flow) {
        self.activity = activity
        self.flow = flow
    }

    var body: some View {
        let colorOf = colors()
        let spentColor = Theme.color.onSurfaceVariant
        let small = Theme.type.labelSmall
        let pending = activity.state == .pending
        // Each bucket shows its notes, or one bar for its part if none made yet.
        let shown = flow.out.map { b in (b, b.notes.isEmpty ? [b.amount] : b.notes) }
        let most = max((flow.spent + shown.flatMap { $0.1 }).map { bits(amount: $0) }.max() ?? 1, 1)
        let leftRows = max(flow.spent.count, 1)
        let rightRows = shown.reduce(0) { $0 + $1.1.count }
        let height = max(HEAD + ROW * CGFloat(leftRows), HEAD * CGFloat(shown.count) + ROW * CGFloat(rightRows))
        let described = {
            var said = "Spent " + (flow.spent.isEmpty ? "none" : flow.spent.map { digits(amount: $0) }.joined(separator: ", ")) + ". "
            for (b, notes) in shown {
                said += shortTo(kind: activity.kind, to: b.to) + ": " + notes.map { digits(amount: $0) }.joined(separator: ", ") + ". "
            }
            return said
        }()
        Text("Each bar is a note: twice as large, a step longer.")
            .textStyle(Theme.type.bodySmall)
            .foregroundStyle(Theme.color.onSurfaceVariant)
            .padding(.top, 8)
            .padding(.bottom, 8)
        Canvas { context, size in
            let w = size.width
            // Spent on the left, made on the right, the lines between.
            let leftEdge = w * 0.36
            let rightEdge = w * 0.56
            let longest = min(leftEdge, w - rightEdge) - LABEL
            func length(amount: UInt64) -> CGFloat { longest * CGFloat(max(bits(amount: amount), 1)) / CGFloat(most) }
            let row = ROW
            let text: CGFloat = 3

            // Spent: bars ending at the column's edge, amounts outside.
            var y = (size.height - (HEAD + ROW * CGFloat(leftRows))) / 2
            let heading = flow.spent.isEmpty
                ? from(kind: activity.kind) : "Spent · \(sat(amount: flow.spent.reduce(0, &+)))"
            label(context: context, text: heading, style: small, color: spentColor, x: 0, y: y + text, width: leftEdge)
            y += HEAD
            var ends: [CGPoint] = []
            if flow.spent.isEmpty { ends.append(CGPoint(x: leftEdge, y: y + row / 2)) }
            for amount in flow.spent {
                let len = length(amount: amount)
                bar(context: context, at: CGPoint(x: leftEdge - len, y: y + (row - BAR) / 2), length: len, color: spentColor, alpha: 0.8)
                label(
                    context: context, text: digits(amount: amount), style: small, color: spentColor,
                    x: 0, y: y + text, width: leftEdge - len - 4, end: true
                )
                ends.append(CGPoint(x: leftEdge, y: y + row / 2))
                y += row
            }
            let middle = CGPoint(x: (leftEdge + rightEdge) / 2, y: size.height / 2)
            for end in ends { curve(context: context, a: end, b: middle, color: spentColor.opacity(0.45)) }

            // Made, by whom: a heading, then bars from the column's edge, amounts outside.
            y = (size.height - (HEAD * CGFloat(shown.count) + ROW * CGFloat(rightRows))) / 2
            for (bucket, notes) in shown {
                let color = colorOf(bucket.to)
                let title = "\(shortTo(kind: activity.kind, to: bucket.to)) · \(sat(amount: bucket.amount))"
                label(context: context, text: title, style: small, color: color, x: rightEdge, y: y + text, width: w - rightEdge)
                y += HEAD
                let faint = pending && bucket.notes.isEmpty
                for amount in notes {
                    let len = length(amount: amount)
                    curve(context: context, a: middle, b: CGPoint(x: rightEdge, y: y + row / 2), color: color.opacity(faint ? 0.25 : 0.55))
                    bar(context: context, at: CGPoint(x: rightEdge, y: y + (row - BAR) / 2), length: len, color: color, alpha: faint ? 0.35 : 1)
                    label(
                        context: context, text: digits(amount: amount), style: small, color: color,
                        x: rightEdge + len + 4, y: y + text, width: w - rightEdge - len - 4
                    )
                    y += row
                }
            }
            context.fill(Path(ellipseIn: CGRect(x: middle.x - 3, y: middle.y - 3, width: 6, height: 6)), with: .color(spentColor))
        }
        .frame(maxWidth: .infinity)
        .frame(height: height)
        .accessibilityElement()
        .accessibilityLabel(described)
    }
}

private func bar(context: GraphicsContext, at: CGPoint, length: CGFloat, color: Color, alpha: Double) {
    let rect = CGRect(x: at.x, y: at.y, width: length, height: BAR)
    context.fill(Path(roundedRect: rect, cornerRadius: 4), with: .color(color.opacity(alpha)))
}

/// A line from `a` to `b`, leaving and arriving level.
private func curve(context: GraphicsContext, a: CGPoint, b: CGPoint, color: Color) {
    let mid = (a.x + b.x) / 2
    var path = Path()
    path.move(to: a)
    path.addCurve(to: b, control1: CGPoint(x: mid, y: a.y), control2: CGPoint(x: mid, y: b.y))
    context.stroke(path, with: .color(color), lineWidth: 1.5)
}

/// `text` on one line at most `width` wide from `x`, or ending at `x + width` if `end`.
private func label(
    context: GraphicsContext,
    text: String,
    style: TextStyle,
    color: Color,
    x: CGFloat,
    y: CGFloat,
    width: CGFloat,
    end: Bool = false
) {
    if width <= 0 { return }
    func laid(shown: String) -> GraphicsContext.ResolvedText {
        context.resolve(
            Text(shown).font(.system(size: style.size, weight: style.weight)).tracking(style.tracking).foregroundStyle(color)
        )
    }
    let unbounded = CGSize(width: CGFloat.infinity, height: CGFloat.infinity)
    // Cut short with an ellipsis where it does not fit.
    var shown = text
    var resolved = laid(shown: shown)
    while resolved.measure(in: unbounded).width > width && !shown.isEmpty {
        shown.removeLast()
        resolved = laid(shown: shown + "…")
    }
    let left = end ? x + width - resolved.measure(in: unbounded).width : x
    context.draw(resolved, at: CGPoint(x: left, y: y), anchor: .topLeading)
}
