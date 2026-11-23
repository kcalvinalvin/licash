import BackgroundTasks
import SwiftUI

@main
struct LicashApp: App {
    @UIApplicationDelegateAdaptor(AppDelegate.self) private var delegate

    var body: some Scene {
        WindowGroup {
            MainView(engine: AppDelegate.engine)
        }
    }
}

/// The app as iOS launches it, in sight or for a look out of sight: its
/// periodic looks registered before launching ends, as iOS requires.
final class AppDelegate: NSObject, UIApplicationDelegate {
    /// The one wallet engine, for every screen.
    static let engine = Engine()

    func application(_ application: UIApplication, didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil) -> Bool {
        BGTaskScheduler.shared.register(forTaskWithIdentifier: WatchJob.ID, using: nil) { task in
            Task { @MainActor in WatchJob().onStartJob(params: task) }
        }
        BGTaskScheduler.shared.register(forTaskWithIdentifier: ScanJob.ID, using: nil) { task in
            Task { @MainActor in ScanJob().onStartJob(params: task) }
        }
        return true
    }
}
