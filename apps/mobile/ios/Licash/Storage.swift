import Foundation

/// What the app keeps on the phone, in bytes, by what keeps it: the wallet,
/// Tor's state, the light client of each network
/// it ran on, the logs, caches, and the rest of its data. The app itself, its
/// code and libraries, as its bundle holds them.
struct Storage {
    var wallet: Int64
    var tor: Int64
    var chains: [(String, Int64)]
    var logs: Int64
    var cache: Int64
    var other: Int64
    var app: Int64?

    var chain: Int64 { chains.reduce(0) { $0 + $1.1 } }
    var total: Int64 { wallet + tor + chain + logs + cache + other + (app ?? 0) }
}

/// What the app keeps on the phone now. Slow: off the main thread.
func storage() -> Storage {
    let files = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
    // Where an earlier version kept it, too, should it not have moved.
    let oldTor = size(dir: files.appendingPathComponent("wallet/tor"))
    let tor = size(dir: files.appendingPathComponent("tor")) + oldTor
    let found = (try? FileManager.default.contentsOfDirectory(at: files, includingPropertiesForKeys: [.isDirectoryKey])) ?? []
    let chains = found
        .filter { (try? $0.resourceValues(forKeys: [.isDirectoryKey]).isDirectory) == true && $0.lastPathComponent.hasPrefix("chain-") }
        .sorted { $0.lastPathComponent < $1.lastPathComponent }
        .map { (String($0.lastPathComponent.dropFirst("chain-".count)), size(dir: $0)) }
    var parts = Storage(
        wallet: size(dir: files.appendingPathComponent("wallet")) - oldTor,
        tor: tor,
        chains: chains,
        logs: size(dir: files.appendingPathComponent("logs")),
        cache: size(dir: FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]) +
            size(dir: FileManager.default.temporaryDirectory),
        other: 0,
        app: appBytes()
    )
    // The rest of the data directory: settings, databases of libraries.
    let data = size(dir: URL(fileURLWithPath: NSHomeDirectory()))
    let counted = parts.wallet + parts.tor + parts.chain + parts.logs + parts.cache
    parts.other = max(data - counted, 0)
    return parts
}

/// The bytes of the regular files under `dir`, its links not followed.
private func size(dir: URL) -> Int64 {
    if !FileManager.default.fileExists(atPath: dir.path) { return 0 }
    var failed: Error?
    let paths = FileManager.default.enumerator(at: dir, includingPropertiesForKeys: [.isRegularFileKey, .fileSizeKey]) { _, e in
        failed = e
        return false
    }
    var bytes: Int64 = 0
    while let path = paths?.nextObject() as? URL {
        let values = try? path.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey])
        if values?.isRegularFile == true { bytes += Int64(values?.fileSize ?? 0) }
    }
    if let failed {
        AppLog.warn(line: "measuring \(dir.lastPathComponent)", e: failed)
        return 0
    }
    return bytes
}

/// The app's code and libraries: its bundle on the phone.
private func appBytes() -> Int64 { size(dir: Bundle.main.bundleURL) }
