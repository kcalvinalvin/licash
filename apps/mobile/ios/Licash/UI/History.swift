import LicashCore
import SwiftUI

/// Activity: what came in and went out, the newest first, what is on its
/// way before it. A line says what, when and how much. A tap, the rest.
/// Pulled down, it looks for payments now.
struct History: View {
    let engine: Engine
    let ui: Ui
    let open: (Activity) -> Void
    let stats: () -> Void
    let back: () -> Void

    var body: some View {
        let now = Int64(Date().timeIntervalSince1970)
        PullToLook(engine: engine) {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    TopBar(title: "Activity", back: back) {
                        IconButton(onClick: stats) {
                            StatsGlyph.frame(width: 24, height: 24).accessibilityLabel("Stats")
                        }
                        .accessibilityIdentifier("stats-open")
                    }
                    if ui.history.isEmpty && ui.incoming.isEmpty {
                        Text("Nothing yet")
                            .textStyle(Theme.type.bodyLarge)
                            .foregroundStyle(Theme.color.onSurfaceVariant)
                            .padding(.horizontal, 4)
                            .padding(.vertical, 24)
                    }
                    ForEach(ui.incoming, id: \.id) { activity in
                        ActivityRow(activity: activity, now: now, coming: true) { open(activity) }
                    }
                    ForEach(ui.history, id: \.key) { activity in
                        ActivityRow(activity: activity, now: now, contact: ui.contacts[activity.key]) { open(activity) }
                    }
                }
            }
            .padding(.horizontal, 20)
            .accessibilityIdentifier("history")
        }
    }
}

/// A line of the history: an icon of its kind and direction, what it was,
/// its day, and its amount. One under way or failed says so in place of
/// its day.
private struct ActivityRow: View {
    let activity: Activity
    let now: Int64
    var coming: Bool = false
    var contact: String? = nil
    let open: () -> Void

    var body: some View {
        let colors = Theme.color
        let failed = !coming && activity.state == .failed
        let under = coming || activity.state == .pending
        let sign = if activity.kind == .swap {
            ""
        } else if !coming && paid(activity: activity) {
            "−"
        } else {
            "+"
        }
        let tint = if failed {
            colors.error
        } else if under {
            colors.onSurfaceVariant
        } else if sign == "+" {
            colors.primary
        } else if activity.kind == .swap {
            colors.secondary
        } else {
            colors.onSurfaceVariant
        }
        Button(action: open) {
            HStack(alignment: .center, spacing: 0) {
                ZStack(alignment: .center) {
                    Circle().fill(tint.opacity(0.14))
                    glyph(activity: activity, coming: coming, failed: failed)
                        .resizable()
                        .scaledToFit()
                        .frame(width: 20, height: 20)
                        .foregroundStyle(tint)
                }
                .frame(width: 40, height: 40)
                Spacer().frame(width: 14)
                VStack(alignment: .leading, spacing: 0) {
                    Text(title(activity: activity, coming: coming))
                        .textStyle(Theme.type.bodyLarge)
                        .lineLimit(1)
                    // Whom it was paid to, or by: a contact.
                    if let it = counterpart(kind: activity.kind, contact: contact) {
                        Text(it)
                            .textStyle(Theme.type.bodySmall)
                            .foregroundStyle(colors.onSurfaceVariant)
                            .lineLimit(1)
                            .accessibilityIdentifier("row-contact")
                    }
                    // What it says it was for, as its payer or payee read it.
                    if let memo = activity.memo {
                        Text(memo)
                            .textStyle(Theme.type.bodySmall)
                            .foregroundStyle(colors.onSurfaceVariant)
                            .lineLimit(1)
                            .accessibilityIdentifier("row-memo")
                    }
                    // The separate code it was paid at.
                    if let code = activity.code {
                        Text("To \(code)")
                            .textStyle(Theme.type.bodySmall)
                            .foregroundStyle(colors.onSurfaceVariant)
                            .lineLimit(1)
                            .accessibilityIdentifier("row-code")
                    }
                    if failed {
                        Text("Failed").textStyle(Theme.type.bodySmall).foregroundStyle(colors.error)
                    } else if under {
                        Text("Pending")
                            .textStyle(Theme.type.bodySmall)
                            .foregroundStyle(colors.tertiary)
                            .accessibilityIdentifier("pending")
                    } else if let at = activity.at {
                        Text(day(seconds: at, now: now))
                            .textStyle(Theme.type.bodySmall)
                            .foregroundStyle(colors.onSurfaceVariant)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                Spacer().frame(width: 12)
                Text(
                    activity.kind == .optimized ? optimizationCost(fee: activity.fee, state: activity.state)
                        : "\(sign)\(sat(amount: activity.amount))"
                )
                .textStyle(Amounts)
                .foregroundStyle(
                    failed ? colors.error
                        : under ? colors.onSurfaceVariant
                        : sign == "+" ? colors.primary
                        : colors.onSurface
                )
            }
            .padding(.horizontal, 4)
            .padding(.vertical, 8)
            .frame(maxWidth: .infinity, minHeight: 64, alignment: .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}

/// The icon of a line of the history: failed, or its kind and direction.
private func glyph(activity: Activity, coming: Bool, failed: Bool) -> ImageVector {
    if failed {
        Image(systemName: "exclamationmark.triangle.fill")
    } else if coming {
        Glyphs.down
    } else {
        switch activity.kind {
        case .lightning, .invoice: Glyphs.bolt
        case .swap, .optimized: Glyphs.swap
        case .contract, .closed: Glyphs.shield
        case .sent, .payout: Glyphs.up
        case .received, .moved, .funded, .onchain: Glyphs.down
        }
    }
}
