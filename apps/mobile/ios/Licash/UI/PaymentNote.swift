import LicashCore
import SwiftUI

/// The most bytes of a payment's note, in UTF-8, as the wallet takes it.
let MAX_NOTE_BYTES = 200

/// What is typed as a note: its words, one space apart, none if blank.
func noteOf(typed: String) -> String? {
    let note = typed.split(whereSeparator: \.isWhitespace).joined(separator: " ")
    return note.isEmpty ? nil : note
}

/// `typed`, cut to what a note holds: whole characters, ``MAX_NOTE_BYTES`` in UTF-8 at most.
func fitNote(typed: String) -> String {
    var bytes = 0
    var kept = String.UnicodeScalarView()
    for point in typed.unicodeScalars {
        let size = String(point).utf8.count
        if bytes + size > MAX_NOTE_BYTES { break }
        bytes += size
        kept.append(point)
    }
    return String(kept)
}

/// The most characters of an extra code's label, as the wallet takes it.
let MAX_CODE_LABEL = 40

/// What is typed as an extra code's label: its words, one space apart, none if blank.
func labelOf(typed: String) -> String? {
    let words = typed.split(whereSeparator: \.isWhitespace).joined(separator: " ")
    // Counted in code points, as the wallet counts them.
    let label = String(words.unicodeScalars.prefix(MAX_CODE_LABEL))
    return label.isEmpty ? nil : label
}

/// The name of the code `index` among `codes`: 0 the wallet's own.
func codeName(index: UInt32, codes: [ExtraCode]) -> String {
    index == 0 ? MAIN_IDENTITY : codes.first { $0.index == index }?.label ?? "Identity \(index)"
}

/// A note for the payee, and whether to tell them a code of this wallet's,
/// `shareAs` (0 its own, else one of its extra `codes`): sealed for the
/// payee alone, neither read by the hub nor the relays. Sharing a code tells
/// them who paid, and lets them pay back or keep it as a contact.
struct NoteFields: View {
    let note: String
    let onNote: (String) -> Void
    let share: Bool
    let onShare: (Bool) -> Void
    var codes: [ExtraCode] = []
    var shareAs: UInt32 = 0
    var onShareAs: (UInt32) -> Void = { _ in }
    @SwiftUI.State private var picking = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            OutlinedTextField(
                value: note,
                onValueChange: { onNote(fitNote(typed: $0)) },
                label: "Note (optional)",
                singleLine: true,
                shape: RoundedRectangle(cornerRadius: 14)
            )
            .textInputAutocapitalization(.sentences)
            .submitLabel(.done)
            .accessibilityIdentifier("note-field")
            Spacer().frame(height: 4)
            Text("Only they can read it.")
                .textStyle(Theme.type.bodySmall)
                .foregroundStyle(Theme.color.onSurfaceVariant)
            Spacer().frame(height: 8)
            Toggle(
                title: "Share my code",
                on: share,
                sub: share ? "They'll know it's you" : "They won't know who paid",
                means: "Tells them your payment code, so they know the payment is from you and can pay you back. " +
                    "Anyone they show it to can link it to you.",
                set: onShare
            )
            if share && !codes.isEmpty {
                Entry(
                    title: "Share as",
                    value: codeName(index: shareAs, codes: codes),
                    tag: "share-as",
                    onClick: { picking = true }
                )
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .sheet(isPresented: $picking) {
            Sheet(title: "Share which code", dismiss: { picking = false }) {
                Entry(title: MAIN_IDENTITY, sub: "The one your contacts know", tag: "share-as-0") {
                    picking = false
                    onShareAs(0)
                }
                ForEach(codes, id: \.index) { code in
                    Entry(title: code.label, sub: "A separate code", tag: "share-as-\(code.index)") {
                        picking = false
                        onShareAs(code.index)
                    }
                }
            }
        }
    }
}
