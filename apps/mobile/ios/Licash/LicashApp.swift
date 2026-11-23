import Chain
import LicashCore
import SwiftUI

@main
struct LicashApp: App {
    var body: some Scene {
        WindowGroup {
            Text("\(chainName(network: .bitcoin)) pinned at \(MobilePinHeight("bitcoin"))")
                .accessibilityIdentifier("placeholder")
        }
    }
}
