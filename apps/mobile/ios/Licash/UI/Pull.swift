import SwiftUI

/// `content`, which pulled down from its top looks for payments now rather
/// than at the next round ([Engine.lookNow]): spinning until it has. Its
/// scroll view takes the pull.
struct PullToLook<Content: View>: View {
    let engine: Engine
    let content: Content

    init(engine: Engine, @ViewBuilder content: () -> Content) {
        self.engine = engine
        self.content = content()
    }

    var body: some View {
        ZStack(alignment: .topLeading) { content }
            .refreshable { await engine.lookNow() }
            .accessibilityElement(children: .contain)
            .accessibilityIdentifier("pull-to-look")
    }
}
