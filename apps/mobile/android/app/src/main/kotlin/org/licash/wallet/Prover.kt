package org.licash.wallet

import android.app.ActivityManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import java.io.File
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.runInterruptible

/*
 * A forced close's claim proved on the phone itself: licash-r0, installed
 * with the app as liblicash_r0.so when built with ../prover.sh, run as a
 * process of its own from a foreground service, its work kept on disk so
 * that any stop (killed, out of memory, the phone restarted) carries on
 * where it stopped. It makes the succinct (STARK) receipt only: its
 * Groth16 wrap needs an x86 computer, the receipt saved and wrapped there.
 *
 * Each proof is a directory of proofs/<key>, a contract's id or "sample":
 * statement.input, guests/native-claim.bin (the program its contract
 * names), po2 (its segments' size, kept for its work to stay valid), work/
 * (licash-r0's, work/receipt once made), prove.log, and paused once the
 * user stopped it.
 */

/** A proof's share done of a join, against a segment's: joins take a third of the time. */
private const val JOIN = 0.3

/** How far a proof on the phone is, as its log and its files tell. */
data class Proving(
    /** Its segments, 0 until its statement is executed. */
    val segments: Int = 0,
    /** Segments proved, and joins of them made (segments - 1 in all). */
    val lifted: Int = 0,
    val joined: Int = 0,
    /** Whether it runs now, was stopped by the user, or made its receipt. */
    val running: Boolean = false,
    val paused: Boolean = false,
    val done: Boolean = false,
    /** Why its last run stopped, if it failed. */
    val error: String? = null,
    /** The last runs' pace: seconds a segment and a join took, once one did. */
    val segmentSecs: Double? = null,
    val joinSecs: Double? = null,
) {
    /** Its share done, 0 to 1. */
    val share: Float
        get() = when {
            done -> 1f
            segments == 0 -> 0f
            else -> ((lifted + JOIN * joined) / (segments + JOIN * (segments - 1))).toFloat()
        }

    /** Seconds left at its pace, null until known. */
    val secsLeft: Long?
        get() {
            if (done) return 0
            val segment = segmentSecs ?: return null
            if (segments == 0) return null
            val join = joinSecs ?: (segment * JOIN)
            return ((segments - lifted) * segment + (segments - 1 - joined).coerceAtLeast(0) * join).toLong()
        }
}

/** The line the service writes before each run, and after it with its exit code. */
internal const val RUN = "== run"
internal const val EXIT = "== exit"

private val EXECUTED = Regex("""executed in [\d.]+ s, \d+ user cycles, (\d+) segments""")
private val CARRIED = Regex("""carried on, (\d+) of (\d+) segments lifted already""")
private val SEGMENT = Regex("""segment (\d+)/(\d+) proved and lifted in ([\d.]+) s""")
private val JOINED = Regex("""joined (\d+)/(\d+) in ([\d.]+) s""")

/** How many of the last steps set a proof's pace. */
private const val PACE = 10

/** A proof's log read, every run's: how far it got, at what pace, and why its last run stopped. Pure. */
internal fun progressOf(lines: List<String>): Proving {
    var p = Proving()
    val segments = ArrayDeque<Double>()
    val joins = ArrayDeque<Double>()
    fun ArrayDeque<Double>.took(secs: Double) {
        addLast(secs)
        if (size > PACE) removeFirst()
    }
    for (line in lines) {
        EXECUTED.find(line)?.let { p = p.copy(segments = it.groupValues[1].toInt()) }
        CARRIED.find(line)?.let {
            val (lifted, n) = it.destructured
            p = p.copy(segments = n.toInt(), lifted = maxOf(p.lifted, lifted.toInt()))
        }
        SEGMENT.find(line)?.let {
            val (i, n, secs) = it.destructured
            p = p.copy(segments = n.toInt(), lifted = maxOf(p.lifted, i.toInt()))
            segments.took(secs.toDouble())
        }
        JOINED.find(line)?.let {
            val (i, n, secs) = it.destructured
            p = p.copy(segments = n.toInt(), lifted = n.toInt(), joined = maxOf(p.joined, i.toInt() - 1))
            joins.took(secs.toDouble())
        }
        when {
            line.startsWith(RUN) -> p = p.copy(error = null)
            "succinct receipt" in line -> p = p.copy(done = true, error = null)
            line.startsWith("Error:") -> p = p.copy(error = line.removePrefix("Error:").trim())
            line.startsWith(EXIT) -> {
                val code = line.removePrefix(EXIT).trim()
                if (code != "0" && p.error == null) p = p.copy(error = "the prover quit (exit $code)")
            }
        }
    }
    return p.copy(segmentSecs = segments.takeIf { it.isNotEmpty() }?.average(), joinSecs = joins.takeIf { it.isNotEmpty() }?.average())
}

/**
 * The segments' size for a phone of `memory` bytes, as large as it holds: 2^18 cycles took 2.4 GB at
 * most to prove (and lift), 2^16 1.5 GB, and the smaller the segments, the more of them, and the more
 * work in all (the holder's claim: 4,475 segments of 2^16, 92 of 2^20).
 */
internal fun po2For(memory: Long): Int = when {
    memory >= 6L shl 30 -> 18
    memory >= 4L shl 30 -> 17
    else -> 16
}

/** How long `secs` take, as a clock would say it. */
internal fun lasting(secs: Long): String {
    val minutes = (secs + 30) / 60
    val hours = (minutes + 30) / 60
    return when {
        minutes < 1 -> "under a minute"
        minutes < 60 -> "$minutes min"
        hours == 1L -> "1 hour"
        else -> "$hours hours"
    }
}

/** Where a proof on the phone stands, in words. Pure. */
internal fun provingWords(p: Proving): String {
    val left = p.secsLeft?.let { ", about ${lasting(it)} left" } ?: ""
    return when {
        p.done -> "Proved on this phone: its receipt is ready"
        p.paused && !p.running -> "Paused at ${(p.share * 100).toInt()}%"
        p.error != null && !p.running -> "Stopped: ${p.error}"
        p.segments == 0 -> if (p.running) "Reading what to prove" else "Waiting to start"
        p.lifted < p.segments -> "Proving part ${p.lifted + 1} of ${p.segments}$left"
        else -> "Joining the parts, ${p.joined + 1} of ${p.segments - 1}$left"
    }
}

/** The proofs made on this phone, and the prover that makes them, if this build carries it. */
class Prover(context: Context) {
    private val app = context.applicationContext
    private val binary = File(app.applicationInfo.nativeLibraryDir, BINARY)
    private val root = File(app.filesDir, "proofs")
    private val state = MutableStateFlow<Map<String, Proving>>(emptyMap())

    /** Each proof's progress, by its key, as last looked at. */
    val proofs: StateFlow<Map<String, Proving>> = state.asStateFlow()

    /** Whether this build carries the prover (../prover.sh). */
    val available: Boolean = binary.canExecute()

    /** Whether it carries a sample statement, for a developer to see how long a proof takes here. */
    val sample: Boolean = available && hasAsset("r0/sample", "statement.input")

    /** The run under way in this process, if any. */
    @Volatile
    private var current: Process? = null

    init {
        look()
    }

    private fun hasAsset(dir: String, name: String) = runCatching { app.assets.list(dir)?.contains(name) == true }
        .getOrDefault(false)

    private fun dir(key: String) = File(root, key)

    /** The receipt proof `key` made, once made. */
    fun receipt(key: String): File? = File(dir(key), "work/receipt").takeIf { it.isFile }

    /**
     * Set contract `key`'s proof up, to run: its `statement` and the program its contract names, its
     * image id `program` in hex, among those the app carries. A statement other than its work's starts
     * over.
     */
    fun prepare(key: String, statement: ByteArray, program: String) {
        check(hasAsset("r0/programs", "$program.bin")) { "this app does not carry the program its contract names" }
        val dir = dir(key)
        val input = File(dir, "statement.input")
        if (input.isFile && !input.readBytes().contentEquals(statement)) dir.deleteRecursively()
        set(dir, "r0/programs/$program.bin", statement)
    }

    /** Set the sample proof up, to run. */
    fun prepareSample() {
        set(dir(SAMPLE), "r0/sample/native-claim.bin", app.assets.open("r0/sample/statement.input").use { it.readBytes() })
    }

    private fun set(dir: File, guest: String, statement: ByteArray) {
        File(dir, "guests").mkdirs()
        File(dir, "tmp").mkdirs()
        val input = File(dir, "statement.input")
        if (!input.isFile) write(input, statement)
        val program = File(dir, "guests/native-claim.bin")
        if (!program.isFile) write(program, app.assets.open(guest).use { it.readBytes() })
        val po2 = File(dir, "po2")
        if (!po2.isFile) {
            val memory = ActivityManager.MemoryInfo()
            (app.getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager).getMemoryInfo(memory)
            write(po2, po2For(memory.totalMem).toString().toByteArray())
        }
        File(dir, "paused").delete()
        look()
    }

    /** `bytes` kept at `file` whole or not at all. */
    private fun write(file: File, bytes: ByteArray) {
        val part = File(file.parentFile, file.name + ".part")
        part.writeBytes(bytes)
        check(part.renameTo(file)) { "keeping ${file.name}" }
    }

    /** The proofs to run: set up, neither made nor stopped by the user. */
    fun pending(): List<String> = keys().filter {
        val dir = dir(it)
        File(dir, "guests/native-claim.bin").isFile && receipt(it) == null && !File(dir, "paused").exists()
    }

    private fun keys(): List<String> = root.listFiles()?.filter { File(it, "statement.input").isFile }
        ?.map { it.name }?.sorted().orEmpty()

    /**
     * Run proof `key` until it ends or stops, blocking: the service's. A run left by an earlier process
     * of the app is waited for, not doubled. Whether its receipt is made.
     */
    fun prove(key: String): Boolean {
        val dir = dir(key)
        val log = File(dir, "prove.log")
        running(dir)?.let { pid ->
            AppLog.info("proof $key runs already, as $pid")
            while (File("/proc/$pid").exists()) Thread.sleep(WAIT_MS)
            look()
            return receipt(key) != null
        }
        val po2 = File(dir, "po2").readText().trim()
        val cpus = Runtime.getRuntime().availableProcessors()
        log.appendText("$RUN ${System.currentTimeMillis() / 1000} po2 $po2, $cpus cpus\n")
        // Through a shell that keeps its pid, for a later process of the app to tell it runs.
        val command = listOf(
            "/system/bin/sh", "-c", "echo \$\$ > pid; exec \"\$0\" native-prove-succinct statement.input work",
            binary.path,
        )
        val builder = ProcessBuilder(command).directory(dir).redirectErrorStream(true)
            .redirectOutput(ProcessBuilder.Redirect.appendTo(log))
        builder.environment().apply {
            put("LD_LIBRARY_PATH", binary.parent)
            put("LICASH_R0_GUESTS", File(dir, "guests").path)
            put("TMPDIR", File(dir, "tmp").path)
            put("LICASH_SEGMENT_PO2", po2)
        }
        AppLog.info("proving $key on this phone, segments of 2^$po2 cycles")
        val process = builder.start()
        current = process
        val code = try {
            process.waitFor()
        } catch (e: InterruptedException) {
            process.destroy()
            throw e
        } finally {
            current = null
        }
        log.appendText("$EXIT $code\n")
        File(dir, "pid").delete()
        look()
        AppLog.info("proof $key stopped: exit $code")
        return receipt(key) != null
    }

    /** The pid of proof `dir`'s prover, if one runs: perhaps of an earlier process of the app. */
    private fun running(dir: File): Int? {
        val pid = runCatching { File(dir, "pid").readText().trim().toInt() }.getOrNull() ?: return null
        val command = runCatching { File("/proc/$pid/cmdline").readText() }.getOrNull() ?: return null
        return pid.takeIf { BINARY in command }
    }

    /** Stop every proof, each to carry on at the user's word only (set up again). */
    fun stop() {
        for (key in keys()) {
            val dir = dir(key)
            if (receipt(key) == null) File(dir, "paused").createNewFile()
            running(dir)?.let { android.os.Process.sendSignal(it, SIGTERM) }
        }
        current?.destroy()
        look()
    }

    /** Proof `key` and its work forgotten. */
    fun forget(key: String) {
        val dir = dir(key)
        running(dir)?.let { android.os.Process.sendSignal(it, SIGTERM) }
        dir.deleteRecursively()
        look()
    }

    /** Each proof's progress read again. */
    fun look() {
        state.value = keys().associateWith { key ->
            val dir = dir(key)
            val log = File(dir, "prove.log")
            progressOf(if (log.isFile) log.readLines() else emptyList()).copy(
                running = running(dir) != null,
                paused = File(dir, "paused").exists(),
                done = receipt(key) != null,
            )
        }
    }

    companion object {
        private const val BINARY = "liblicash_r0.so"
        const val SAMPLE = "sample"
        private const val WAIT_MS = 5_000L
        private const val SIGTERM = 15
    }
}

/**
 * Runs the proofs set up on this phone one after another, in the
 * foreground, the CPU kept awake, until none is left to run. Started at
 * the user's word, while the app is in sight; a run killed carries on
 * where it stopped once started again.
 */
class ProverService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private var proving: Job? = null
    private var wake: PowerManager.WakeLock? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val engine = (application as LicashApp).engine
        val prover = engine.prover
        if (intent?.action == STOP) {
            proving?.cancel()
            prover.stop()
            stopSelf()
            return START_NOT_STICKY
        }
        try {
            ServiceCompat.startForeground(
                this,
                Notifier.PROVING_ID,
                engine.notifier.proving(null),
                if (Build.VERSION.SDK_INT >= 34) ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE else 0,
            )
        } catch (e: Exception) {
            AppLog.warn("proving in the foreground", e)
            stopSelf()
            return START_NOT_STICKY
        }
        if (proving == null) {
            val power = getSystemService(POWER_SERVICE) as PowerManager
            wake = power.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "licash:proving").apply { acquire(MOST_MS) }
            proving = scope.launch {
                val showing = launch {
                    while (true) {
                        prover.look()
                        val shown = prover.proofs.value.values.firstOrNull { it.running }
                        engine.notifier.showProving(shown)
                        delay(LOOK_MS)
                    }
                }
                // A run that fails without getting further is not run again: the user may, later.
                val failed = mutableSetOf<String>()
                while (true) {
                    val key = prover.pending().firstOrNull { it !in failed } ?: break
                    val before = prover.proofs.value[key]
                    val made = runInterruptible(Dispatchers.IO) { prover.prove(key) }
                    val after = prover.proofs.value[key]
                    if (!made && (after == null || after.share <= (before?.share ?: 0f))) failed += key
                }
                showing.cancel()
                prover.look()
                AppLog.info("nothing left to prove")
                stopSelf()
            }
        }
        return START_STICKY
    }

    override fun onDestroy() {
        proving?.cancel()
        wake?.takeIf { it.isHeld }?.release()
        scope.cancel()
        super.onDestroy()
    }

    companion object {
        private const val STOP = "org.licash.wallet.STOP_PROVING"
        private const val LOOK_MS = 5_000L

        /** The longest it keeps the CPU awake for in one go: a proof may take a day. */
        private const val MOST_MS = 36 * 3600 * 1000L

        /** Run the proofs set up: only while the app is in sight. */
        fun start(context: Context) {
            runCatching { ContextCompat.startForegroundService(context, Intent(context, ProverService::class.java)) }
                .onFailure { AppLog.warn("starting to prove", it) }
        }

        /** Stop every proof, to carry on at the user's word. */
        fun stop(context: Context) {
            runCatching { context.startService(Intent(context, ProverService::class.java).setAction(STOP)) }
                .onFailure {
                    AppLog.warn("stopping the proofs", it)
                    (context.applicationContext as LicashApp).engine.prover.stop()
                }
        }
    }
}
