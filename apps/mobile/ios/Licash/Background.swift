import BackgroundTasks
import Foundation
import Network
import UIKit
import UserNotifications

// The app at work out of sight, as far as iOS lets it: a background task
// while a payment is under way, begun while the app is in sight and given
// some seconds more once it leaves, and periodic tasks iOS runs when it
// chooses, should the background task end. None runs with the phone off or
// once the app is closed from the app switcher.

/// The app's notifications: watching, payments, and deadlines.
final class Notifier: NSObject, UNUserNotificationCenterDelegate {
    private let manager = UNUserNotificationCenter.current()

    override init() {
        super.init()
        // No channels on iOS: each notification carries its channel's
        // importance ([builder]), and shows in sight too.
        manager.delegate = self
    }

    /// The background task's own, while it works: finishing payments, or
    /// else catching up with Bitcoin, at the step `ui` is at.
    func watching(ui: Ui? = nil) -> UNMutableNotificationContent {
        let b = builder(channel: Notifier.WATCHING)
        let step = ui.flatMap { u in u.watch != .now && behind(ui: u) ? stages(ui: u).first { $0.state != .done } : nil }
        guard let step else {
            b.title = "Finishing payments under way"
            b.body = "Licash keeps at it until they are done."
            return b
        }
        // No progress bar on iOS: the step's words tell how far.
        b.title = "Catching up with Bitcoin"
        b.body = "\(step.name): \(step.detail)"
        return b
    }

    /// The background task's notification shown anew.
    func working(notification: UNNotificationContent) {
        manager.getNotificationSettings { settings in
            if settings.authorizationStatus != .authorized { return }
            let request = UNNotificationRequest(identifier: "\(Notifier.WATCHING_ID)", content: notification, trigger: nil)
            self.manager.add(request) { e in
                if let e { AppLog.warn(line: "a notification", e: e) }
            }
        }
    }

    /// Money in, or a payment finished: `key` names it, each told once.
    func payment(key: String, text: String) {
        let b = builder(channel: Notifier.PAYMENTS)
        b.title = text
        post(tag: key, id: Notifier.PAYMENT_ID, notification: b)
    }

    /// A payment needing the app open soon: told now, or in `after` seconds
    /// should the app not run by then to tell it, as iOS may not. Told again,
    /// it replaces the one waiting.
    func deadline(key: String, text: String, after: TimeInterval? = nil) {
        let b = builder(channel: Notifier.DEADLINES)
        b.title = "Open Licash soon"
        b.body = text
        let trigger = after.map { UNTimeIntervalNotificationTrigger(timeInterval: max(1, $0), repeats: false) }
        post(tag: key, id: Notifier.DEADLINE_ID, notification: b, trigger: trigger)
    }

    /// The deadline of `key` waiting to be told, no more: its payment claimed or finished.
    func cancelDeadline(key: String) {
        manager.removePendingNotificationRequests(withIdentifiers: ["\(Notifier.DEADLINE_ID) \(key)"])
    }

    /// Its words off the lock screen unless she shows previews there always:
    /// what the wallet does is for its owner only. Tapped, it opens the app
    /// as left, and is gone.
    private func builder(channel: String) -> UNMutableNotificationContent {
        let n = UNMutableNotificationContent()
        n.threadIdentifier = channel
        // Its channel's importance, as how it interrupts.
        switch channel {
        case Notifier.WATCHING:
            n.interruptionLevel = .passive
        case Notifier.DEADLINES:
            n.interruptionLevel = .timeSensitive
            n.sound = .default
        default:
            n.interruptionLevel = .active
            n.sound = .default
        }
        return n
    }

    private func post(tag: String, id: Int, notification: UNMutableNotificationContent, trigger: UNNotificationTrigger? = nil) {
        manager.getNotificationSettings { settings in
            if settings.authorizationStatus != .authorized { return }
            let request = UNNotificationRequest(identifier: "\(id) \(tag)", content: notification, trigger: trigger)
            self.manager.add(request) { e in
                if let e { AppLog.warn(line: "a notification", e: e) }
            }
        }
    }

    /// In sight too: listed, and as a banner with its sound, but watching's,
    /// listed only.
    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async -> UNNotificationPresentationOptions {
        notification.request.content.threadIdentifier == Notifier.WATCHING ? [.list] : [.banner, .list, .sound]
    }

    static let WATCHING = "watching"
    static let PAYMENTS = "payments"
    static let DEADLINES = "deadlines"
    static let WATCHING_ID = 1
    private static let PAYMENT_ID = 2
    private static let DEADLINE_ID = 3
}

/// Keeps the engine at work while a payment is under way, or while it
/// catches up with Bitcoin, until neither: catching up for an hour at most.
/// Begun while the app is in sight: once it leaves, iOS gives it some
/// seconds, then ends it.
@MainActor
final class WatchService {
    private var watching: Task<Void, Never>?
    private var task = UIBackgroundTaskIdentifier.invalid

    private func onStartCommand() {
        let engine = AppDelegate.engine
        task = UIApplication.shared.beginBackgroundTask(withName: "watching") {
            MainActor.assumeIsolated { self.onTimeout() }
        }
        if task == .invalid {
            // Not allowed now: the periodic look remains.
            AppLog.warn(line: "watching in the background")
            return
        }
        WatchService.running = true
        engine.keep(on: true)
        AppLog.info(line: engine.ui.watch == .now ? "watching payments under way" : "catching up out of sight")
        watching = Task {
            let began = ContinuousClock.now
            var shown: String?
            var done = false
            // At most 6 hours, in sight: a line stuck under way leaves it to the periodic look.
            while ContinuousClock.now - began < .milliseconds(WatchService.MOST_MS) {
                let ui = engine.ui
                let n = engine.notifier.watching(ui: ui)
                // Its text tells the step and how far: shown anew when it changes.
                if n.body != shown {
                    shown = n.body
                    engine.notifier.working(notification: n)
                }
                // A light client that cannot catch up does not keep it at work for ever.
                let catching = behind(ui: ui) && ContinuousClock.now - began < .milliseconds(WatchService.CATCH_UP_MS)
                if ui.watch != .now && !catching {
                    done = true
                    break
                }
                // Looked at again each second.
                do { try await Task.sleep(for: .seconds(1)) } catch { return }
            }
            AppLog.info(line: done ? "nothing left to watch or catch up" : "watched for long enough")
            stopSelf()
        }
    }

    /// Out of the time iOS gives out of sight: stop, as it asks.
    private func onTimeout() {
        AppLog.info(line: "out of time watching in the background")
        stopSelf()
    }

    /// The engine no longer kept at work for it, its notification gone, its
    /// background task ended.
    private func stopSelf() {
        if task == .invalid { return }
        if watching != nil { AppDelegate.engine.keep(on: false) }
        watching?.cancel()
        WatchService.running = false
        UNUserNotificationCenter.current().removeDeliveredNotifications(withIdentifiers: ["\(Notifier.WATCHING_ID)"])
        UIApplication.shared.endBackgroundTask(task)
        task = .invalid
    }

    private static let MOST_MS: Int64 = 6 * 3600 * 1000
    private static let CATCH_UP_MS: Int64 = 3600 * 1000

    private static var running = false

    /// Watch now, if not yet.
    static func start() {
        if running { return }
        WatchService().onStartCommand()
    }
}

/// A look every 15 minutes or so while anything is under way: should the
/// background task end, or the app be stopped. It keeps the engine at work
/// for a round or two in sync with Bitcoin, at most [ROUNDS_MS]. iOS decides
/// when, and how often, it runs.
@MainActor
final class WatchJob {
    private var looking: Task<Void, Never>?
    private let network = NWPathMonitor()

    func onStartJob(params: BGTask) {
        let engine = AppDelegate.engine
        // Again at its next period: iOS runs a request once.
        WatchJob.schedule()
        params.expirationHandler = { Task { @MainActor in self.onStopJob(params: params) } }
        // Told of the network it starts on too: a poke then changes nothing.
        network.pathUpdateHandler = { [weak self] _ in Task { @MainActor in self?.onNetworkChanged(params: params) } }
        network.start(queue: .main)
        looking = Task {
            await engine.round(ms: WatchJob.ROUNDS_MS)
            if looking == nil { return }
            looking = nil
            network.cancel()
            params.setTaskCompleted(success: true)
        }
    }

    func onStopJob(params: BGTask) {
        guard let round = looking else { return }
        round.cancel()
        looking = nil
        network.cancel()
        // Again at its next period, scheduled as it started.
        params.setTaskCompleted(success: false)
    }

    /// Another network while looking: the round goes on over it, a sync started now rather than later.
    func onNetworkChanged(params: BGTask) {
        AppDelegate.engine.poke()
    }

    static let ID = "org.licash.wallet.watch"
    private static let PERIOD_MS: Int64 = 15 * 60 * 1000
    private static let ROUNDS_MS: Int64 = 4 * 60 * 1000

    /// Whether looking is asked for: a cancel while a schedule waits on iOS wins.
    private static var wanted = false

    /// Look now and then, if not yet.
    static func schedule() {
        wanted = true
        Task {
            if await BGTaskScheduler.shared.pendingTaskRequests().contains(where: { $0.identifier == ID }) { return }
            if !wanted { return }
            let job = BGAppRefreshTaskRequest(identifier: ID)
            job.earliestBeginDate = Date(timeIntervalSinceNow: Double(PERIOD_MS) / 1000)
            do {
                try BGTaskScheduler.shared.submit(job)
            } catch {
                AppLog.warn(line: "scheduling a look now and then", e: error)
            }
        }
    }

    static func cancel() {
        wanted = false
        BGTaskScheduler.shared.cancel(taskRequestWithIdentifier: ID)
    }
}

/// A look every [PERIOD_MS] or so while the phone rests (with a network),
/// whatever is under way: a round in sync with Bitcoin, which looks for the
/// wallet's coins up to the chain's height, so that coins paid to it are
/// found without the app being opened. At most [ROUNDS_MS]. iOS decides
/// when, and how often, it runs.
@MainActor
final class ScanJob {
    private var looking: Task<Void, Never>?
    private let network = NWPathMonitor()

    func onStartJob(params: BGTask) {
        let engine = AppDelegate.engine
        // Again at its next period: iOS runs a request once.
        ScanJob.schedule()
        params.expirationHandler = { Task { @MainActor in self.onStopJob(params: params) } }
        // Told of the network it starts on too: a poke then changes nothing.
        network.pathUpdateHandler = { [weak self] _ in Task { @MainActor in self?.onNetworkChanged(params: params) } }
        network.start(queue: .main)
        looking = Task {
            await engine.round(ms: ScanJob.ROUNDS_MS)
            if looking == nil { return }
            looking = nil
            network.cancel()
            params.setTaskCompleted(success: true)
        }
    }

    func onStopJob(params: BGTask) {
        guard let round = looking else { return }
        round.cancel()
        looking = nil
        network.cancel()
        params.setTaskCompleted(success: false)
    }

    /// As [WatchJob.onNetworkChanged].
    func onNetworkChanged(params: BGTask) {
        AppDelegate.engine.poke()
    }

    static let ID = "org.licash.wallet.scan"
    private static let PERIOD_MS: Int64 = 4 * 3600 * 1000
    private static let ROUNDS_MS: Int64 = 10 * 60 * 1000

    /// Whether looking is asked for: a cancel while a schedule waits on iOS wins.
    private static var wanted = false

    /// Look while the phone rests, if not yet.
    static func schedule() {
        wanted = true
        Task {
            if await BGTaskScheduler.shared.pendingTaskRequests().contains(where: { $0.identifier == ID }) { return }
            if !wanted { return }
            let job = BGProcessingTaskRequest(identifier: ID)
            job.earliestBeginDate = Date(timeIntervalSinceNow: Double(PERIOD_MS) / 1000)
            job.requiresNetworkConnectivity = true
            do {
                try BGTaskScheduler.shared.submit(job)
            } catch {
                AppLog.warn(line: "scheduling a look while resting", e: error)
            }
        }
    }

    static func cancel() {
        wanted = false
        BGTaskScheduler.shared.cancel(taskRequestWithIdentifier: ID)
    }
}
