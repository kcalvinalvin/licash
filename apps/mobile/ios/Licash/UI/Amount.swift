import SwiftUI

/// The most digits an amount in sat takes: 21 million bitcoin is 16.
private let MOST_DIGITS = 16

/// What is typed, as an amount's digits: no leading zeros, at most [MOST_DIGITS].
func amountDigits(typed: String) -> String {
    let digits = String(String.UnicodeScalarView(typed.unicodeScalars.filter { $0.properties.generalCategory == .decimalNumber }))
    return String(digits.drop { $0 == "0" }.prefix(MOST_DIGITS))
}

/// `digits` as shown, thousands apart (`12,345`), and where each of its
/// offsets lands in it: `at[i]` for the offset before digit `i`, `at[n]` past
/// the last.
final class Grouped {
    let text: String
    let at: [Int]

    init(text: String, at: [Int]) {
        self.text = text
        self.at = at
    }
}

func grouped(digits: String) -> Grouped {
    var out = ""
    let chars = Array(digits)
    var at = [Int](repeating: 0, count: chars.count + 1)
    for i in chars.indices {
        if i > 0 && (chars.count - i) % 3 == 0 { out.append(",") }
        at[i] = out.count
        out.append(chars[i])
    }
    at[chars.count] = out.count
    return Grouped(text: out, at: at)
}

/// An amount in sat, typed straight in where it shows, big: focused with its
/// keyboard up as it first shows. `done`, if any, is the keyboard's action
/// (else it only hides the keyboard): never a payment, which a button makes.
/// The number pad has no action key: a button above it, named `action`,
/// stands for it.
struct AmountField: View {
    let onAmount: (Int64?) -> Void
    let action: String
    let done: (() -> Void)?
    // The digits typed, thousands apart.
    @State private var shown: String
    @FocusState private var focused: Bool

    init(amount: Int64?, onAmount: @escaping (Int64?) -> Void, action: String = "Done", done: (() -> Void)? = nil) {
        self.onAmount = onAmount
        self.action = action
        self.done = done
        _shown = State(initialValue: grouped(digits: amount.map { String($0) } ?? "").text)
    }

    var body: some View {
        // What is typed: the digits shown, without their commas.
        let digits = shown.filter { $0 != "," }
        let faint = Theme.color.onSurfaceVariant.opacity(0.4)
        // Smaller as it grows, to stay on one line on a narrow phone.
        let size: CGFloat =
            switch grouped(digits: digits).text.count + 4 {
            case 0...9: 56
            case 10...11: 44
            case 12...14: 36
            default: 28
            }
        let style = { var style = Figures; style.size = size; style.lineHeight = size + 4; return style }()
        // The digits typed shown as an amount: `12,345 sat`, the cursor among the digits.
        HStack(alignment: .firstTextBaseline, spacing: 0) {
            TextField("", text: $shown, prompt: Text("0").foregroundStyle(faint))
                .fixedSize()
                .focused($focused)
                .keyboardType(.numberPad)
                .onSubmit { submit() }
                .accessibilityIdentifier("amount")
            Text(" sat").foregroundStyle(digits.isEmpty ? faint : Theme.color.onSurface)
        }
        .textStyle(style)
        .foregroundStyle(Theme.color.onSurface)
        .tint(Theme.color.primary)
        .frame(maxWidth: .infinity)
        .contentShape(Rectangle())
        .onTapGesture { focused = true }
        .onChange(of: shown) {
            let d = amountDigits(typed: shown)
            // The cursor kept at the end: an amount is typed, not edited inside.
            shown = grouped(digits: d).text
            let amount = d.unicodeScalars.reduce(Int64(0)) { $0 * 10 + Int64(Character($1).wholeNumberValue ?? 0) }
            onAmount(amount > 0 ? amount : nil)
        }
        .toolbar {
            ToolbarItemGroup(placement: .keyboard) {
                Spacer()
                Button(action) { submit() }
            }
        }
        .task { focused = true }
    }

    private func submit() {
        if let done { done() } else { focused = false }
    }
}
