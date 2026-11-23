import Chain
import Foundation
import LicashCore
import os

/// The app's log: to the system's log, to a file moved aside once it reaches [MAX]
/// bytes, and its newest lines for the developer screen. The wallet core
/// logs through it. The light client writes a file of its own beside it.
enum AppLog {
    private static let MAX: Int64 = 1 << 20
    private static let KEEP = 300
    private static var newest: [String] = []
    private static var file: URL?
    private static let lock = NSLock()
    private static let system = os.Logger(subsystem: "org.licash.wallet", category: "licash")

    /// The log as the core's `Logger`.
    static let instance: LicashCore.Logger = AppLogger()

    /// Log to files in `dir` from now on, the light client's too.
    static func start(dir: URL) {
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        lock.lock()
        file = dir.appendingPathComponent("app.log")
        lock.unlock()
        var error: NSError?
        if !MobileSetLog(dir.appendingPathComponent("chain.log").path, &error) {
            warn(line: "the light client's log", e: error)
        }
    }

    static func log(level: String, line: String) {
        let type: OSLogType = switch level {
        case "ERROR": .error
        case "WARN": .default
        case "DEBUG": .debug
        case "TRACE": .debug
        default: .info
        }
        system.log(level: type, "\(line, privacy: .public)")
        let stamped = "\(Date.ISO8601FormatStyle(includingFractionalSeconds: true).format(Date())) " +
            "\(level + String(repeating: " ", count: max(0, 5 - level.count))) \(line)"
        lock.lock()
        defer { lock.unlock() }
        newest.append(stamped)
        while newest.count > KEEP { newest.removeFirst() }
        if let file { write(f: file, line: stamped) }
    }

    static func info(line: String) { log(level: "INFO", line: line) }

    static func warn(line: String, e: Error? = nil) {
        log(level: "WARN", line: e.map { "\(line): \(explain(e: $0))" } ?? line)
    }

    static func recent() -> [String] {
        lock.lock()
        defer { lock.unlock() }
        return newest
    }

    private static func write(f: URL, line: String) {
        let size = ((try? FileManager.default.attributesOfItem(atPath: f.path))?[.size] as? NSNumber)?.int64Value ?? 0
        if size > MAX { rename(f.path, f.path + ".1") }
        do {
            if !FileManager.default.fileExists(atPath: f.path) { FileManager.default.createFile(atPath: f.path, contents: nil) }
            let out = try FileHandle(forWritingTo: f)
            defer { try? out.close() }
            try out.seekToEnd()
            try out.write(contentsOf: Data((line + "\n").utf8))
        } catch {}
    }
}

private final class AppLogger: LicashCore.Logger {
    func log(level: String, line: String) { AppLog.log(level: level, line: line) }
}

/// The logs as one file to share, headed by what helps reading them.
func exportLogs(ui: Ui) -> URL {
    let dir = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0].appendingPathComponent("logs")
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    let out = dir.appendingPathComponent("licash-logs.txt")
    let logs = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0].appendingPathComponent("logs")
    let info = Bundle.main.infoDictionary
    let version = info?["CFBundleShortVersionString"] as? String ?? ""
    let build = info?["CFBundleVersion"] as? String ?? ""
    let commit = Bundle.main.object(forInfoDictionaryKey: "LicashCommit") as? String ?? "unknown"
    let release = ProcessInfo.processInfo.operatingSystemVersion
    var machine = utsname()
    uname(&machine)
    let model = withUnsafeBytes(of: machine.machine) { bytes in String(decoding: bytes.prefix { $0 != 0 }, as: UTF8.self) }
    var w = "Licash \(version) (\(build), \(commit)) on iOS " +
        "\(release.majorVersion).\(release.minorVersion).\(release.patchVersion), Apple \(model)\n"
    let chain = ui.chain
    w += "\("\(ui.network)".uppercased()): block \(chain.height), \(chain.peers) peers, " +
        (chain.synced ? "synced" : "syncing") +
        ", looked in to \(ui.scan.map { "\($0.next)" } ?? "none"), \(ui.hubs.count) hubs, tor \(ui.tor)\n"
    if let problem = ui.problem { w += "problem: \(problem)\n" }
    for name in ["app.log", "chain.log"] {
        w += "\n"
        w += "== \(name)\n"
        for f in [logs.appendingPathComponent("\(name).1"), logs.appendingPathComponent(name)] {
            guard let data = FileManager.default.contents(atPath: f.path) else { continue }
            String(decoding: data, as: UTF8.self).enumerateLines { line, _ in w += line + "\n" }
        }
    }
    try? w.write(to: out, atomically: true, encoding: .utf8)
    return out
}
