import Accessibility
import LicashCore
import SwiftUI
import UIKit
import UserNotifications

private indirect enum Screen: Equatable {
    case home
    case receive

    /// Sending, or with `cashOut`, a hub, paying notes there out: all of
    /// them, or as much as chosen, `amount` suggested. `own` is the address
    /// of this wallet's that `link` pays, if it does.
    case send(link: String?, cashOut: String? = nil, from: Screen = .home, own: String? = nil, amount: UInt64? = nil)
    case settings

    /// Where the money is, opened from `from`: all of it, or `at` one
    /// place, a hub's URL or what is on Bitcoin.
    case money(from: Screen = .home, at: String? = nil)

    /// The history: what came in and went out.
    case history

    /// What the history came to: counts, fees and times, by period.
    case stats

    /// One line of the history, opened from `from`.
    case details(activity: Activity, from: Screen = .history)

    /// What its page is told apart by: its kind, not what it shows.
    var contentKey: String {
        switch self {
        case .home: "home"
        case .receive: "receive"
        case .send: "send"
        case .settings: "settings"
        case .money: "money"
        case .history: "history"
        case .stats: "stats"
        case .details: "details"
        }
    }
}

/// The screen back goes to from `screen`.
private func parent(screen: Screen) -> Screen {
    switch screen {
    case let .send(_, _, from, _, _): return from
    case let .money(from, at): return at != nil ? .money(from: from, at: nil) : from
    case let .details(_, from): return from
    case .stats: return .history
    default: return .home
    }
}

/// How deep in the app `screen` is: Home, a page of it, what a page opens.
private func depth(screen: Screen) -> Int {
    switch screen {
    case .home: return 0
    case .details, .stats: return 2
    case let .send(_, _, from, _, _):
        if case .money = from { return 2 }
        return 1
    default: return 1
    }
}

/// How one screen gives way to another: a fade, and a short slide the way
/// it goes, deeper from the right and back from the left. Reduce Motion
/// ends it ([MainView.move]).
private func moving(initial: Screen, target: Screen, width: CGFloat) -> AnyTransition {
    let way: CGFloat = depth(screen: target) >= depth(screen: initial) ? 1 : -1
    return .asymmetric(
        insertion: AnyTransition.opacity.animation(.easeInOut(duration: 0.21).delay(0.07))
            .combined(with: AnyTransition.offset(x: width / 14 * way).animation(.easeInOut(duration: 0.28))),
        removal: AnyTransition.opacity.animation(.easeInOut(duration: 0.07))
            .combined(with: AnyTransition.offset(x: -width / 14 * way).animation(.easeInOut(duration: 0.28)))
    )
}

/// The app's window: a page at a time, and the links it is opened with.
struct MainView: View {
    let engine: Engine

    /// A `bitcoin:` link the app was opened with, not yet shown.
    @SwiftUI.State private var link: String?
    @SwiftUI.State private var screen: Screen = .home
    /// The screen before [screen], for the way it moved.
    @SwiftUI.State private var previous: Screen = .home
    /// The screen moved to, set before [screen]: the page leaving goes with
    /// the transition it last showed, so it shows this move's first.
    @SwiftUI.State private var target: Screen = .home
    /// Whether the engine was told the app is in sight since it last left.
    @SwiftUI.State private var inSight = false
    /// What the snackbar tells now, if anything.
    @SwiftUI.State private var toast: String?
    @Environment(\.scenePhase) private var phase
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private var ui: Ui { engine.ui }

    var body: some View {
        let watching = ui.watch != Watch.none
        LicashTheme {
            app
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Theme.color.background.ignoresSafeArea())
                .overlay(alignment: .bottom) { snackbar }
                // Counts of blocks are told in the time of the wallet's network.
                .environment(\.blockSeconds, blockSeconds(network: ui.network))
        }
        .onOpenURL { link = $0.absoluteString }
        .task {
            for await e in engine.events() {
                withAnimation { toast = e.text }
                AccessibilityNotification.Announcement(e.text).post()
                try? await Task.sleep(for: .seconds(4))
                withAnimation { toast = nil }
            }
        }
        // Asked once there is a wallet, before its first catching up, to tell of
        // it and of payments out of sight: iOS asks once, then no more.
        .task(id: [ui.wallet, watching]) {
            let center = UNUserNotificationCenter.current()
            let missing = await center.notificationSettings().authorizationStatus != .authorized
            if (ui.wallet || watching) && missing {
                let granted = (try? await center.requestAuthorization(options: [.alert, .sound, .badge])) ?? false
                AppLog.info(line: granted ? "notifications allowed" : "notifications not allowed")
            }
        }
        .onChange(of: ui.wallet, initial: true) { if !ui.wallet { move(to: .home) } }
        .onChange(of: link, initial: true) { showLink() }
        .onChange(of: ui.wallet) { showLink() }
        // In sight and out of it, as the engine keeps to the app's lifecycle.
        .onChange(of: phase, initial: true) {
            switch phase {
            // In sight again only once out of it: not after Notification Center, an alert or a prompt.
            case .active where !inSight:
                inSight = true
                engine.started()
            case .background:
                inSight = false
                // iOS suspends the app seconds after it leaves the screen: the
                // backup begun now gets the time it needs, as far as iOS gives it.
                let backup = engine.stopped()
                let app = UIApplication.shared
                var id = UIBackgroundTaskIdentifier.invalid
                id = app.beginBackgroundTask {
                    app.endBackgroundTask(id)
                    id = .invalid
                }
                Task {
                    await backup.value
                    if id != .invalid { app.endBackgroundTask(id) }
                }
            default: break
            }
        }
    }

    @ViewBuilder
    private var app: some View {
        if !ui.loaded {
            Wordmark(size: 40)
        } else if !ui.wallet {
            Onboarding(engine: engine, ui: ui)
        } else {
            GeometryReader { geo in
                ZStack(alignment: .topLeading) {
                    // A page that only changes what it shows keeps its state, and does not animate.
                    page(of: screen)
                        .id(screen.contentKey)
                        .transition(moving(initial: previous, target: target, width: geo.size.width))
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            }
            // A swipe from the left edge goes back, as across iOS.
            .simultaneousGesture(
                DragGesture(minimumDistance: 20).onEnded { drag in
                    if screen != .home && drag.startLocation.x < 24 && drag.translation.width > 80 {
                        move(to: parent(screen: screen))
                    }
                }
            )
        }
    }

    @ViewBuilder
    private func page(of s: Screen) -> some View {
        switch s {
        case .home:
            Home(
                engine: engine,
                ui: ui,
                receive: { move(to: .receive) },
                send: { move(to: .send(link: nil)) },
                settings: { move(to: .settings) },
                money: { move(to: .money(at: $0)) },
                history: { move(to: .history) },
                open: { move(to: .details(activity: $0, from: .home)) }
            )
        case .history:
            History(
                engine: engine,
                ui: ui,
                open: { move(to: .details(activity: $0)) },
                stats: { move(to: .stats) }
            ) { move(to: .home) }
        case .stats:
            StatsScreen(engine: engine, ui: ui) { move(to: .history) }
        case .receive:
            Receive(engine: engine) { move(to: .home) }
        case let .send(link, cashOut, from, own, amount):
            Send(
                engine: engine,
                ui: ui,
                link: link,
                cashOut: cashOut,
                ownAddress: own,
                cashOutAmount: amount,
                toReceive: { move(to: .receive) }
            ) { move(to: from) }
        case .settings:
            Settings(engine: engine, ui: ui, balance: { move(to: .money(from: .settings)) }) {
                move(to: .home)
            }
        case let .money(from, at):
            Money(
                engine: engine,
                ui: ui,
                at: at,
                go: { move(to: .money(from: from, at: $0)) },
                cashOut: { hub, amount in move(to: .send(link: nil, cashOut: hub, from: s, amount: amount)) },
                payOwn: { link, own in move(to: .send(link: link, from: s, own: own)) }
            ) { move(to: from) }
        case let .details(activity, from):
            ActivityDetails(engine: engine, ui: ui, shown: activity) { move(to: from) }
        }
    }

    @ViewBuilder
    private var snackbar: some View {
        if let toast {
            Text(toast)
                .textStyle(Theme.type.bodyMedium)
                .foregroundStyle(Theme.color.inverseOnSurface)
                .padding(.horizontal, 16)
                .padding(.vertical, 14)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Theme.color.inverseSurface, in: RoundedRectangle(cornerRadius: 4, style: .continuous))
                .padding(12)
                .transition(.opacity)
        }
    }

    /// `next` shown: moved to as [moving] tells, unless only what its page
    /// shows changes, or Reduce Motion is on.
    private func move(to next: Screen) {
        let moves = next.contentKey != screen.contentKey && !reduceMotion
        previous = screen
        target = next
        if moves {
            DispatchQueue.main.async { withAnimation { screen = next } }
        } else {
            screen = next
        }
    }

    /// The link the app was opened with shown, once there is a wallet.
    private func showLink() {
        if let link, ui.wallet {
            move(to: .send(link: link))
            self.link = nil
        }
    }
}
