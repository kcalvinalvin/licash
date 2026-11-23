import LicashCore
import SwiftUI

// How well each hub's notes are protected: what its contracts cover of what
// is held there, what they leave out, and the room they have for more.
// Pure, to be tested without a screen.

/// One hub's notes and their protection, in sat: `held` there, `covered` by
/// its contracts protecting now, and `caps`, the most those could cover.
/// `notes`, how many there are.
struct HubCover: Equatable {
    var held: UInt64
    var covered: UInt64
    var caps: UInt64
    var notes: UInt64

    /// Held there and not covered: on the hub's word alone.
    var missing: UInt64 { held > covered ? held - covered : 0 }

    /// How much more its contracts could cover, were more held there.
    var room: UInt64 { caps > covered ? caps - covered : 0 }

    /// In a line: how much is covered, then what is missing or the room
    /// left. Nothing to say while nothing is held or protects.
    var says: String? {
        if held == 0 && caps == 0 { return nil }
        if caps == 0 { return "Not protected" }
        if missing > 0 && covered == 0 { return "\(sat(amount: missing)) not protected" }
        if missing > 0 { return "\(digits(amount: covered)) of \(sat(amount: held)) protected, \(sat(amount: missing)) not" }
        if room > 0 { return "All protected, room for \(sat(amount: room)) more" }
        return "All protected, no room for more"
    }

    /// The notes held there, in words: `3 notes`.
    var noteWords: String { notes == 1 ? "1 note" : "\(digits(amount: notes)) notes" }
}

/// What `contracts` protect at `hub` of what `holding` holds there, and how
/// many of `notes` are there.
func hubCover(hub: String, holding: Holding?, contracts: [Contract], notes: [Notes]) -> HubCover {
    let held = holding.map { $0.available + $0.pending } ?? 0
    let protecting = contracts.filter { sameHub(a: $0.hub, b: hub) && $0.state == .protecting }
    let covered = min(protecting.reduce(0) { $0 + $1.covers }, held)
    return HubCover(
        held: held,
        covered: covered,
        caps: protecting.reduce(0) { $0 + $1.cap },
        notes: notes.filter { sameHub(a: $0.hub, b: hub) }.reduce(0) { $0 + $1.count }
    )
}

/// `now` once a contract covering up to `cap` opens, `funded` of it hers, joining what is held.
func coverAfter(now: HubCover, cap: UInt64, funded: UInt64) -> HubCover {
    let held = now.held + funded
    var after = now
    after.held = held
    after.covered = min(held, now.covered + cap)
    after.caps = now.caps + cap
    return after
}

/// The least a contract at `hub` protects, if it opens them: below that,
/// what is not protected is not worth a contract of its own.
func smallestContract(hub: Hub?) -> UInt64? {
    guard let terms = hub?.contracts else { return nil }
    return terms.range?.minCap ?? terms.cap
}

/// Whether to suggest protecting or cashing out what `cover` leaves
/// unprotected at `hub`: once it reaches the hub's smallest contract, unless
/// the owner dismissed it at `dismissed` sat or more.
func suggestProtecting(cover: HubCover, hub: Hub?, dismissed: UInt64?) -> Bool {
    guard let smallest = smallestContract(hub: hub) else { return false }
    return cover.missing >= smallest && (dismissed == nil || cover.missing > dismissed!)
}

/// What [suggestProtecting] says.
func unprotectedWords(missing: UInt64, hub: String) -> String {
    "\(sat(amount: missing)) at \(name(url: hub)) is not protected. Protect it, or cash part of it out to Bitcoin."
}

/// [HubCover] as a bar: covered, then missing, then the room left, each to
/// scale of the largest of what is held and what its contracts could cover.
struct CoverBar: View {
    let cover: HubCover

    var body: some View {
        let whole = max(cover.held, cover.covered + cover.room)
        if whole != 0 {
            let colors = Theme.color
            let parts = [
                (cover.covered, colors.primary),
                (cover.missing, colors.error),
                (cover.room, colors.primaryContainer),
            ].filter { $0.0 > 0 }
            let weights = parts.map { Double($0.0) / Double(whole) }
            // Each weight its share of them all, as a row's weights are.
            let total = weights.reduce(0, +)
            GeometryReader { g in
                HStack(spacing: 0) {
                    ForEach(parts.indices, id: \.self) { i in
                        parts[i].1.frame(width: g.size.width * weights[i] / total)
                    }
                }
            }
            .frame(height: 6)
            .background(colors.surfaceContainerHighest)
            .clipShape(RoundedRectangle(cornerRadius: 3))
            .frame(maxWidth: .infinity)
            .accessibilityIdentifier("hub-cover-bar")
        }
    }
}
