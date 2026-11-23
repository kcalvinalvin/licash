import LicashCore
import SwiftUI

/// The kinds of address an xpub's wallet may use: native segwit or taproot.
let COLD_SCRIPTS = [("wpkh", "Native segwit (bc1q)"), ("tr", "Taproot (bc1p)")]

/// Whether `key` needs the kind of its addresses chosen: an xpub or tpub
/// names either, a zpub or vpub native segwit alone.
func needsScript(key: String) -> Bool {
    let k = key.trimmingCharacters(in: .whitespacesAndNewlines)
    return k.hasPrefix("xpub") || k.hasPrefix("tpub")
}

/// Rules typed in, if each is: a threshold, an amount kept below it, a fee limit above 0% and at most 100%.
struct ColdForm: Equatable {
    var threshold: UInt64
    var keep: UInt64
    var maxFeePercent: Double
}

func coldForm(threshold: String, keep: String, percent: String) -> ColdForm? {
    guard let t = UInt64(typed: threshold.trimmingCharacters(in: .whitespacesAndNewlines).replacingOccurrences(of: ",", with: "")) else { return nil }
    let kept = keep.trimmingCharacters(in: .whitespacesAndNewlines).replacingOccurrences(of: ",", with: "")
    guard let k = UInt64(typed: kept.isEmpty ? "0" : kept) else { return nil }
    guard let p = Double(percent.trimmingCharacters(in: .whitespacesAndNewlines)) else { return nil }
    if k > t || !(p > 0.0 && p <= 100.0) { return nil }
    return ColdForm(threshold: t, keep: k, maxFeePercent: p)
}

extension FixedWidthInteger {
    /// The number `text` is, its decimal digits of any script read as ASCII
    /// ones: a keyboard set to Arabic, say, types its own.
    init?(typed text: String) {
        self.init(String(text.unicodeScalars.map { c in
            guard c.properties.numericType == .decimal, let d = Character(c).wholeNumberValue else { return Character(c) }
            return Character(String(d))
        }))
    }
}

/// A fee limit in percent, written out: below 0.001 in E notation, as 5.0E-4.
private func written(percent: Double) -> String {
    let s = "\(percent)"
    if percent >= 1e-3 { return s }
    // Swift writes 0.000123 down to 0.0001, and 1.23e-05 below.
    let parts = s.split(separator: "e")
    let digits = parts.count == 2 ? parts[0].filter { $0 != "." } : s.dropFirst(5)
    let power = parts.count == 2 ? Int(parts[1]) ?? 0 : -4
    return "\(digits.prefix(1)).\(digits.count > 1 ? digits.dropFirst() : "0")E\(power)"
}

private let COLD_MEANS =
    "Once your notes exceed the threshold, all but what you keep is swapped out to your own wallet " +
    "(cold storage) through a swap provider, within your fee limit, each time to a new address of it. " +
    "Licash only sees its public key: it can count what is there, never spend it."

private let BRING_BACK =
    "To bring funds back, send them from your cold wallet to a Bitcoin address shown in Receive. " +
    "Licash finds the coin and offers to protect it or move it into notes."

/// Cold storage: what the external wallet holds and where the next
/// withdrawal goes, with its rules, or a form to set it up.
struct ColdPage: View {
    let engine: Engine
    let ui: Ui
    @SwiftUI.State private var editing = false
    @SwiftUI.State private var error: String? = nil

    var body: some View {
        if ui.cold == nil || editing {
            ColdSetup(engine: engine, before: ui.cold) { editing = false }
        } else if let cold = ui.cold {
            VStack(alignment: .leading, spacing: 0) {
                Group(title: "") {
                    Line(label: "Balance", value: sat(amount: cold.balance), tag: "cold-balance")
                    Divider()
                    Line(label: "Coins", value: "\(cold.coins)")
                    Divider()
                    Line(label: "Checked", value: cold.scanned.map { "from block \(cold.birthday) to \($0)" } ?? "Not yet, on the next sync")
                    if cold.pending != nil {
                        Divider()
                        Line(label: "Withdrawal", value: "Under way")
                    }
                }
                Group(title: "Rules") {
                    Line(label: "Withdraw past", value: sat(amount: cold.threshold))
                    Divider()
                    Line(label: "Keep", value: sat(amount: cold.keep))
                    Divider()
                    Line(label: "Most in fees", value: "\(written(percent: cold.maxFeePercent))%")
                    Divider()
                    Line(label: "Next address", value: cold.next, share: cold.next)
                    Divider()
                    Line(label: "Key", value: cold.key, share: cold.key)
                }
                Group(title: "Bringing funds back") { Say(text: BRING_BACK) }
                BigButton(text: "Change rules", quiet: true) { editing = true }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .accessibilityIdentifier("cold-edit")
                Spacer().frame(height: 12)
                BigButton(text: "Stop withdrawing", danger: true) {
                    Task { if case .failure(let e) = await engine.clearCold() { error = explain(e: e) } }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .accessibilityIdentifier("cold-clear")
                if let error { Problem(text: error) }
            }
            // Each time it shows, after the form too, nothing has failed yet.
            .onAppear { error = nil }
        }
    }
}

private struct ColdSetup: View {
    let engine: Engine
    let before: ColdStorage?
    let done: () -> Void
    @SwiftUI.State private var key: String
    @SwiftUI.State private var script: String
    @SwiftUI.State private var threshold: String
    @SwiftUI.State private var keep: String
    @SwiftUI.State private var percent: String
    @SwiftUI.State private var birthday: String
    @SwiftUI.State private var error: String? = nil

    init(engine: Engine, before: ColdStorage?, done: @escaping () -> Void) {
        self.engine = engine
        self.before = before
        self.done = done
        _key = SwiftUI.State(initialValue: before?.key ?? "")
        _script = SwiftUI.State(initialValue: before?.script ?? "wpkh")
        _threshold = SwiftUI.State(initialValue: before.map { String($0.threshold) } ?? "")
        _keep = SwiftUI.State(initialValue: before.map { String($0.keep) } ?? "0")
        _percent = SwiftUI.State(initialValue: before.map { written(percent: $0.maxFeePercent) } ?? "1")
        _birthday = SwiftUI.State(initialValue: before.map { String($0.birthday) } ?? "")
    }

    var body: some View {
        let form = coldForm(threshold: threshold, keep: keep, percent: percent)
        let typed = birthday.trimmingCharacters(in: .whitespacesAndNewlines)
        let born = typed.isEmpty ? nil : UInt32(typed: typed)
        let bornOk = typed.isEmpty || born != nil
        VStack(alignment: .leading, spacing: 0) {
            Say(text: COLD_MEANS)
            VStack(alignment: .leading, spacing: 4) {
                ColdField(value: key, change: { key = $0; error = nil }, label: "Extended public key", hint: "zpub, or xpub with its kind of address")
                if needsScript(key: key) {
                    Segmented(options: COLD_SCRIPTS.map { $0.1 }, chosen: COLD_SCRIPTS.firstIndex { $0.0 == script } ?? 0) {
                        script = COLD_SCRIPTS[$0].0
                    }
                    Spacer().frame(height: 8)
                }
                ColdField(value: threshold, change: { threshold = $0 }, label: "Withdraw past (sat)", hint: "When your notes exceed this", number: true)
                ColdField(value: keep, change: { keep = $0 }, label: "Keep (sat)", hint: "Left in notes after a withdrawal, at most the threshold", number: true)
                ColdField(value: percent, change: { percent = $0 }, label: "Most in fees (%)", hint: "Of each withdrawal; skipped while it costs more", number: true)
                ColdField(
                    value: birthday,
                    change: { birthday = $0 },
                    label: "Wallet birthday (block, optional)",
                    hint: "Where to start counting its coins; empty for now",
                    number: true,
                    failed: !bornOk
                )
            }
            .padding(.vertical, 12)
            if let error { Problem(text: error) }
            BigButton(text: "Save", enabled: !key.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && form != nil && bornOk) {
                guard let f = form else { return }
                let chosen = needsScript(key: key) ? script : nil
                Task {
                    switch await engine.setCold(
                        key: key.trimmingCharacters(in: .whitespacesAndNewlines),
                        script: chosen,
                        threshold: f.threshold,
                        keep: f.keep,
                        maxFeePercent: f.maxFeePercent,
                        birthday: born
                    ) {
                    case .success: done()
                    case .failure(let e): error = explain(e: e)
                    }
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .accessibilityIdentifier("cold-save")
            if before != nil {
                Spacer().frame(height: 12)
                BigButton(text: "Cancel", quiet: true) { done() }
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }
}

private struct ColdField: View {
    let value: String
    let change: (String) -> Void
    let label: String
    let hint: String
    var number: Bool = false
    var failed: Bool = false

    var body: some View {
        OutlinedTextField(
            value: value,
            onValueChange: change,
            label: label,
            isError: failed,
            supportingText: hint,
            singleLine: true,
            textStyle: Theme.type.bodyMedium,
            shape: RoundedRectangle(cornerRadius: 14)
        )
        // Numbers and punctuation: a point in every region, which the fee limit is read with, and a return key.
        .keyboardType(number ? .numbersAndPunctuation : .asciiCapable)
        .textInputAutocapitalization(.never)
        .submitLabel(.done)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
