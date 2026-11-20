package org.licash.wallet

import android.app.usage.StorageStatsManager
import android.content.Context
import android.os.Process
import java.io.File
import java.nio.file.Files
import java.nio.file.LinkOption

/**
 * What the app keeps on the phone, in bytes, by what keeps it: the wallet,
 * Tor's state, the light client of each network
 * it ran on, the logs, caches, and the rest of its data; the app itself, its
 * code and libraries, as Android counts it, if it says.
 */
data class Storage(
    val wallet: Long,
    val tor: Long,
    val chains: List<Pair<String, Long>>,
    val logs: Long,
    val cache: Long,
    val other: Long,
    val app: Long?,
) {
    val chain: Long get() = chains.sumOf { it.second }
    val total: Long get() = wallet + tor + chain + logs + cache + other + (app ?: 0)
}

/** What the app keeps on the phone now. Slow: off the main thread. */
fun storage(context: Context): Storage {
    val files = context.filesDir
    // Where an earlier version kept it, too, should it not have moved.
    val oldTor = size(File(files, "wallet/tor"))
    val tor = size(File(files, "tor")) + oldTor
    val chains = files.listFiles { f -> f.isDirectory && f.name.startsWith("chain-") }.orEmpty()
        .sortedBy { it.name }
        .map { it.name.removePrefix("chain-") to size(it) }
    val parts = Storage(
        wallet = size(File(files, "wallet")) - oldTor,
        tor = tor,
        chains = chains,
        logs = size(File(files, "logs")),
        cache = size(context.cacheDir) + size(context.codeCacheDir),
        other = 0,
        app = appBytes(context),
    )
    // The rest of the data directory: settings, databases of libraries.
    val data = size(context.dataDir)
    val counted = parts.wallet + parts.tor + parts.chain + parts.logs + parts.cache
    return parts.copy(other = (data - counted).coerceAtLeast(0))
}

/** The bytes of the regular files under `dir`, its links not followed. */
private fun size(dir: File): Long {
    if (!dir.exists()) return 0
    return runCatching {
        Files.walk(dir.toPath()).use { paths ->
            paths.filter { Files.isRegularFile(it, LinkOption.NOFOLLOW_LINKS) }
                .mapToLong { runCatching { Files.size(it) }.getOrDefault(0) }
                .sum()
        }
    }.onFailure { AppLog.warn("measuring ${dir.name}", it) }.getOrDefault(0)
}

/** The app's code and libraries, as Android counts them: its own need no permission. */
private fun appBytes(context: Context): Long? = runCatching {
    val stats = context.getSystemService(StorageStatsManager::class.java)
    stats.queryStatsForUid(context.applicationInfo.storageUuid, Process.myUid()).appBytes
}.onFailure { AppLog.warn("reading the app's size", it) }.getOrNull()
