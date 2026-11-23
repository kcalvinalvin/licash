import LicashCore
import SwiftUI

/// No wallet yet: make one, on Bitcoin unless a test network is chosen
/// first ([networksShown]): the network is the wallet's for good.
struct Onboarding: View {
    let engine: Engine
    let ui: Ui

    init(engine: Engine, ui: Ui) {
        self.engine = engine
        self.ui = ui
    }

    @SwiftUI.State private var testing = false
    @SwiftUI.State private var busy = false
    @SwiftUI.State private var error: String? = nil
    @SwiftUI.State private var restoring = false
    @SwiftUI.State private var wait = Wait()

    var body: some View {
        VStack(alignment: .center, spacing: 0) {
            GeometryReader { box in
                ScrollView {
                    VStack(alignment: .center, spacing: 0) {
                        Wordmark(size: 56)
                        Spacer().frame(height: 12)
                        Text("Bitcoin, paid in a tap")
                            .textStyle(Theme.type.titleMedium)
                            .foregroundStyle(Theme.color.onSurfaceVariant)
                            .multilineTextAlignment(.center)
                        if let unopened = ui.unopened {
                            Spacer().frame(height: 32)
                            Text(UNOPENED).textStyle(Theme.type.titleMedium).multilineTextAlignment(.center)
                            Text(unopenedWords(reason: unopened))
                                .padding(.top, 8)
                                .accessibilityIdentifier("unopened")
                                .textStyle(Theme.type.bodyMedium)
                                .foregroundStyle(Theme.color.onSurfaceVariant)
                                .multilineTextAlignment(.center)
                        }
                        if ui.unopened == nil && networksShown(asked: testing, ui: ui) {
                            Spacer().frame(height: 32)
                            Text("Network").textStyle(Theme.type.labelLarge)
                            FlowRow(spacing: 8) {
                                ForEach([Network.bitcoin, .signet, .mutinynet, .regtest], id: \.self) { network in
                                    FilterChip(
                                        selected: ui.network == network,
                                        onClick: { Task { _ = await engine.setNetwork(network: network) } },
                                        label: named(network: network)
                                    )
                                    .accessibilityIdentifier("network \(network)")
                                }
                            }
                            Text(TEST_NETWORKS)
                                .padding(.top, 8)
                                .textStyle(Theme.type.bodySmall)
                                .foregroundStyle(Theme.color.onSurfaceVariant)
                                .multilineTextAlignment(.center)
                        }
                    }
                    .frame(maxWidth: .infinity, minHeight: box.size.height)
                }
            }
            Spacer().frame(height: 16)
            // A wallet is here: none is made or restored over it.
            if ui.unopened != nil {
                Spacer().frame(height: 24)
            } else {
                if let error { Text(error).textStyle(Theme.type.bodyLarge).foregroundStyle(Theme.color.error).multilineTextAlignment(.center) }
                Waited(wait: wait, center: true).padding(.bottom, 8)
                BigButton(text: busy ? "Creating…" : "Create wallet", enabled: !busy) {
                    busy = true
                    error = nil
                    Task {
                        if case .failure(let e) = await engine.create() { error = explain(e: e) }
                        busy = false
                    }
                }
                .frame(maxWidth: .infinity)
                .accessibilityIdentifier("create")
                TextButton(onClick: { restoring = true }, enabled: !busy) { Text("Restore a wallet") }
                    .padding(.top, 8)
                    .accessibilityIdentifier("restore-wallet")
                // Advanced mode shows the networks anyway.
                let shown = networksShown(asked: testing, ui: ui)
                if !ui.developer {
                    TextButton(
                        onClick: {
                            testing = !shown
                            if shown && ui.network != .bitcoin { Task { _ = await engine.setNetwork(network: .bitcoin) } }
                        },
                        enabled: !busy
                    ) { Text(shown ? "Use Bitcoin" : "Use a test network") }
                    .accessibilityIdentifier("test-network")
                }
                Spacer().frame(height: 24)
            }
        }
        .padding(.horizontal, 32)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .rememberWait(wait: wait, waiting: busy)
        .sheet(isPresented: $restoring) {
            Restore(engine: engine) { restoring = false }
        }
    }
}

/// Chips in rows, each row centered, `spacing` apart and wrapping to the
/// next row once one is full.
private struct FlowRow: Layout {
    let spacing: CGFloat

    init(spacing: CGFloat) {
        self.spacing = spacing
    }

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let rows = rows(width: proposal.width ?? .infinity, subviews: subviews)
        let width = rows.map(\.width).max() ?? 0
        let height = rows.map(\.height).reduce(0, +)
        return CGSize(width: proposal.width ?? width, height: height)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        var y = bounds.minY
        for row in rows(width: bounds.width, subviews: subviews) {
            var x = bounds.minX + (bounds.width - row.width) / 2
            for index in row.indices {
                let size = subviews[index].sizeThatFits(.unspecified)
                subviews[index].place(at: CGPoint(x: x, y: y), proposal: ProposedViewSize(size))
                x += size.width + spacing
            }
            y += row.height
        }
    }

    /// The subviews by row, each row's width and height.
    private func rows(width: CGFloat, subviews: Subviews) -> [(indices: [Int], width: CGFloat, height: CGFloat)] {
        var rows: [(indices: [Int], width: CGFloat, height: CGFloat)] = []
        for index in subviews.indices {
            let size = subviews[index].sizeThatFits(.unspecified)
            if let last = rows.indices.last, rows[last].width + spacing + size.width <= width {
                rows[last].indices.append(index)
                rows[last].width += spacing + size.width
                rows[last].height = max(rows[last].height, size.height)
            } else {
                rows.append((indices: [index], width: size.width, height: size.height))
            }
        }
        return rows
    }
}

/// Whether the networks to choose from show: once asked for, while one
/// other than Bitcoin is chosen (so it is never chosen unseen), or in
/// advanced mode.
func networksShown(asked: Bool, ui: Ui) -> Bool {
    asked || ui.developer || ui.network != .bitcoin
}

/// Said where the wallet on the device could not be opened.
let UNOPENED = "This wallet can't be opened"

/// Why the wallet on the device could not be opened, `reason`, and the way to start over instead.
func unopenedWords(reason: String) -> String {
    var reason = reason
    while reason.hasSuffix(".") { reason.removeLast() }
    return "\(reason). To start over instead, delete this app and install it again: what the wallet holds is then lost, " +
        "unless you saved a backup file."
}

/// What a test network is, said beside them.
let TEST_NETWORKS = "Test networks use coins of no value: Mutinynet's are free from its faucet. " +
    "A wallet keeps the network it is made on."
