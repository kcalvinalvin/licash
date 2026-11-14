package org.licash.wallet

import android.content.Context
import android.content.Intent
import android.os.Build
import android.util.Log
import androidx.core.content.FileProvider
import java.io.File
import java.time.Instant
import org.licash.chain.mobile.Mobile
import org.licash.core.Logger

/**
 * The app's log: to logcat, to a file moved aside once it reaches [MAX]
 * bytes, and its newest lines for the developer screen. The wallet core
 * logs through it; the light client writes a file of its own beside it.
 */
object AppLog : Logger {
    private const val MAX = 1L shl 20
    private const val KEEP = 300
    private val recent = ArrayDeque<String>()
    private var file: File? = null

    /** Log to files in `dir` from now on, the light client's too. */
    fun start(dir: File) {
        dir.mkdirs()
        synchronized(this) { file = File(dir, "app.log") }
        runCatching { Mobile.setLog(File(dir, "chain.log").path) }
            .onFailure { warn("the light client's log", it) }
    }

    override fun log(level: String, line: String) {
        val priority = when (level) {
            "ERROR" -> Log.ERROR
            "WARN" -> Log.WARN
            "DEBUG" -> Log.DEBUG
            "TRACE" -> Log.VERBOSE
            else -> Log.INFO
        }
        Log.println(priority, "licash", line)
        val stamped = "${Instant.now()} ${level.padEnd(5)} $line"
        synchronized(this) {
            recent.addLast(stamped)
            while (recent.size > KEEP) recent.removeFirst()
            file?.let { write(it, stamped) }
        }
    }

    fun info(line: String) = log("INFO", line)

    fun warn(line: String, e: Throwable? = null) =
        log("WARN", if (e == null) line else "$line: ${explain(e)}")

    fun recent(): List<String> = synchronized(this) { recent.toList() }

    private fun write(f: File, line: String) {
        runCatching {
            if (f.length() > MAX) f.renameTo(File(f.path + ".1"))
            f.appendText(line + "\n")
        }
    }
}

/** The logs as one file to share, headed by what helps reading them. */
fun exportLogs(context: Context, ui: Ui): Intent {
    val dir = File(context.cacheDir, "logs").apply { mkdirs() }
    val out = File(dir, "licash-logs.txt")
    val logs = File(context.filesDir, "logs")
    out.bufferedWriter().use { w ->
        w.appendLine(
            "Licash ${BuildConfig.VERSION_NAME} (${BuildConfig.VERSION_CODE}) on Android " +
                "${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT}), ${Build.MANUFACTURER} ${Build.MODEL}",
        )
        val chain = ui.chain
        w.appendLine(
            "${ui.network.name}: block ${chain.height}, ${chain.peers} peers, " +
                (if (chain.synced) "synced" else "syncing") +
                ", looked in to ${ui.scanned ?: "none"}, ${ui.hubs.size} hubs, tor ${ui.tor}",
        )
        ui.problem?.let { w.appendLine("problem: $it") }
        for (name in listOf("app.log", "chain.log")) {
            w.appendLine()
            w.appendLine("== $name")
            for (f in listOf(File(logs, "$name.1"), File(logs, name))) {
                if (f.exists()) f.forEachLine { w.appendLine(it) }
            }
        }
    }
    val uri = FileProvider.getUriForFile(context, "${context.packageName}.logs", out)
    val send = Intent(Intent.ACTION_SEND).apply {
        type = "text/plain"
        putExtra(Intent.EXTRA_STREAM, uri)
        putExtra(Intent.EXTRA_SUBJECT, "Licash logs")
        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
    }
    return Intent.createChooser(send, "Export logs")
}
