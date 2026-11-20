package org.licash.wallet

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.SystemClock
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import java.io.File
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import org.licash.chain.mobile.Chain as LightClient
import org.licash.chain.mobile.Mobile
import org.licash.core.Activity
import org.licash.core.BackupFound
import org.licash.core.BackupStep
import org.licash.core.Balance
import org.licash.core.Code
import org.licash.core.Contact
import org.licash.core.Contract
import org.licash.core.ContractExit
import org.licash.core.ContractQuote
import org.licash.core.ContractState
import org.licash.core.Details
import org.licash.core.DonationAddress
import org.licash.core.ExitConsent
import org.licash.core.FeesRaised
import org.licash.core.FolderFile
import org.licash.core.Holding
import org.licash.core.Hub
import org.licash.core.InvoiceQuote
import org.licash.core.Kind
import org.licash.core.LicashException
import org.licash.core.LightningInvoice
import org.licash.core.MoveQuote
import org.licash.core.Network
import org.licash.core.Notes
import org.licash.core.NotesOf
import org.licash.core.NoteOptimization
import org.licash.core.Paid
import org.licash.core.Recovery
import org.licash.core.PayQuote
import org.licash.core.PayoutQuote
import org.licash.core.HubsFound
import org.licash.core.PayOffer
import org.licash.core.ColdStorage
import org.licash.core.Scan
import org.licash.core.Scanned
import org.licash.core.State
import org.licash.core.TorClient
import org.licash.core.TorStatus
import org.licash.core.Wallet
import org.licash.core.chainName
import org.licash.core.defaultRelays
import org.licash.core.explorer
import org.licash.core.backupFolderName
import org.licash.core.findBackup
import org.licash.core.setLogger
import org.licash.core.walletExists
import org.licash.wallet.ui.Progress
import org.licash.wallet.ui.stages

/** What the screens show. */
data class Ui(
    val loaded: Boolean = false,
    val wallet: Boolean = false,
    val network: Network = Network.BITCOIN,
    val developer: Boolean = false,
    val balance: Balance = Balance(0u, 0u, 0u, 0u, 0u, 0u, 0u),
    val history: List<Activity> = emptyList(),
    val hubs: List<Hub> = emptyList(),
    /** The hubs charging more than agreed as a contract opened there: not paid until approved. */
    val raised: List<FeesRaised> = emptyList(),
    /** Note work waiting for another sync, including privacy rest. */
    val noteMaintenance: Boolean = false,
    val tor: Boolean = false,
    val chain: ChainStatus = ChainStatus(),
    /** How far looking for payments on Bitcoin got, once looking. */
    val scan: Scan? = null,
    /** How far the app's Tor client is in connecting, over Tor. */
    val torStatus: TorStatus? = null,
    /** Payments on Bitcoin on their way: in no block yet. */
    val incoming: List<Activity> = emptyList(),
    /** Why the last round of syncing fell short, if it did. */
    val problem: String? = null,
    /** The recovery contracts with hubs, the newest first. */
    val contracts: List<Contract> = emptyList(),
    /** The contracts forced closed, the oldest first. */
    val recoveries: List<Recovery> = emptyList(),
    /** Each live contract's exit, as the last round left it. */
    val exits: List<ContractExit> = emptyList(),
    /** The hub a contract is being opened at, while one is. */
    val opening: String? = null,
    /**
     * The contract offered for what was paid on Bitcoin, if any: opened only
     * once its owner accepts it, and not offered unasked once `offerDeclined`.
     */
    val offer: ContractQuote? = null,
    val offerDeclined: Boolean = false,
    /** Where each line of the history under way stands, by [key]. */
    val under: Map<String, Details> = emptyMap(),
    /** How closely what is under way is watched, out of sight too. */
    val watch: Watch = Watch.NONE,
    /** When a Lightning invoice was last asked, in seconds since the epoch. */
    val asked: Long? = null,
    /** Payments made still on their way to their payees: kept at their hubs, or to mail. */
    val onTheirWay: UInt = 0u,
    /** Cold storage, if configured: notes withdrawn to an external wallet. */
    val cold: ColdStorage? = null,
    /** Automatic backups, if a folder is chosen. */
    val backup: AutoBackup? = null,
)

/** A line of the history's own key: lines of different kinds can share an id. */
val Activity.key: String get() = "$kind $id"

/** Ordinary progress details are bounded; payout deadlines are always read. */
private const val UNDER_READ = 20

internal fun detailsToRead(history: List<Activity>): List<Activity> = history
    .filter { it.state == State.PENDING }
    .filterIndexed { index, activity -> index < UNDER_READ || activity.kind == Kind.PAYOUT }

/**
 * How the light client stands: the height of its block filters (`height`,
 * what the wallet looks in), of its block headers, and of the best of its
 * peers, which it catches up to (0 if none told).
 */
data class ChainStatus(
    val height: Int = 0,
    val peers: Long = 0,
    val synced: Boolean = false,
    val headers: Int = 0,
    val peerHeight: Int = 0,
    /** How far, in percent, the headers this release pins are downloaded, before the client starts: -1 if not. */
    val importing: Int = -1,
)

/** How `light` stands now. */
private fun statusOf(light: LightClient): ChainStatus = ChainStatus(
    height = light.height(),
    peers = light.peers(),
    synced = light.synced(),
    headers = runCatching { light.headers() }.getOrDefault(0),
    peerHeight = light.peerHeight(),
    importing = light.importing(),
)

/** Automatic backups to a folder: when last written whole, and what failed since, if anything. */
data class AutoBackup(val at: Long?, val problem: String? = null)

/** Something to tell the user once; `received` if it is money in. */
data class Event(val text: String, val received: ULong? = null)

/**
 * A contract's terms its owner chose, within its hub's range: up to `cap`
 * sat over `blocks`, paid from `coin` (`txid:vout`), `funded` sat of it
 * funded by her from that coin, issued back to her balance once its
 * funding confirms; the rest of the coin back on Bitcoin.
 */
data class Chosen(val cap: ULong, val blocks: UInt, val coin: String)

/**
 * The wallet and the light client, and the loop keeping them in step while
 * the app is in sight, or out of sight while a payment is under way (see
 * [Watch]): looking for payments on Bitcoin, offering to protect them,
 * finishing what is under way and taking what arrives in the mailbox. Every wallet call is on an IO thread; those
 * reaching the network or changing what the wallet holds one at a time,
 * those reading it alongside.
 */
class Engine(context: Context) {
    private val app = context.applicationContext
    val prefs = Prefs(context)
    val notifier = Notifier(app)

    private val files = context.filesDir
    private val walletDir = File(files, "wallet").path
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val mutex = Mutex()
    private val lightLock = Mutex()
    private val state = MutableStateFlow(Ui(developer = prefs.developer, network = prefs.network))
    val ui: StateFlow<Ui> = state.asStateFlow()
    private val notices = MutableSharedFlow<Event>(extraBufferCapacity = 16)
    val events: SharedFlow<Event> = notices
    private val wake = Channel<Unit>(Channel.CONFLATED)

    /**
     * Look often: someone is waiting to be paid. In the mailbox too, apart
     * from the rounds, every [MAIL_MS]: its relays kept connected and
     * pushing the new mail, a look takes what they pushed at once, asking
     * them for all of it every 30 s (a few seconds over Tor).
     */
    @Volatile
    var eager = false
        set(on) {
            field = on
            if (on) watchMailbox()
        }

    /** Looking in the mailbox apart from the rounds. */
    private val watchingMailbox = AtomicBoolean(false)

    @Volatile
    private var foreground = false

    /** Those keeping the engine at work out of sight: the service, the periodic job. */
    private val keepers = AtomicInteger()

    /** The rounds done in step with Bitcoin, its light client synced. */
    private val rounds = MutableStateFlow(0L)

    /** Whether the periodic look is scheduled, as last asked; unknown at start. */
    private var looking: Boolean? = null

    /** The events told of out of sight, each in a notification of its own. */
    private val noticed = AtomicInteger()

    /** Every line of the history when last looked, since the app started. */
    private var seen: Set<String>? = null

    /** Whether the wallet was opened and its light client first started, or not. */
    private val started = MutableStateFlow(false)

    @Volatile
    private var wallet: Wallet? = null

    @Volatile
    private var light: LightClient? = null
    private var lightOf: String? = null

    /**
     * When the light client last started ([SystemClock.elapsedRealtime]),
     * and the steps of catching up logged since, each with the time it took.
     */
    private var lightAt = 0L
    private val phases = mutableSetOf<String>()

    /** The app's Tor client, made once first needed over Tor ([tor]). */
    @Volatile
    private var torClient: TorClient? = null

    /** The coins on Bitcoin and payments on their way already told of. */
    private var told: MutableSet<String>? = null

    /** What was on Bitcoin when a contract was last looked for to offer, and when, in ms. */
    private var offeredFor: ULong? = null
    private var offeredAt = 0L

    /** How the light client stood when last logged. */
    private var logged: ChainStatus? = null

    /** Whether what the hubs trusted serve was learned since the app started. */
    @Volatile
    private var hubsLearned = false

    /** When they were last learned, in ms: again every [HUBS_MS], their fees and terms fresh. */
    @Volatile
    private var hubsLearnedAt = 0L

    /**
     * Each hub's contract quote, asked ahead so protecting shows at once,
     * with what was on Bitcoin when asked and when, in ms: fresh for
     * [QUOTE_MS] while that holds ([contractQuote]).
     */
    private val quotes = ConcurrentHashMap<String, Triple<ContractQuote, ULong, Long>>()

    /** Asking the hubs for quotes ahead, apart from the rounds. */
    private val quoting = AtomicBoolean(false)

    /** Carrying on with contracts, apart from the rounds: it may take minutes. */
    private val contracting = AtomicBoolean(false)

    /** Looking for the swap providers announced, apart from the rounds. */
    private val findingProviders = AtomicBoolean(false)

    /** The wallet that last looked for swap providers, and when it looks again ([System.nanoTime]). */
    @Volatile
    private var providersDue: Pair<Wallet, Long>? = null

    init {
        AppLog.start(File(files, "logs"))
        setLogger(AppLog, "info,nostr_sdk=warn,nostr_relay_pool=warn")
        ProcessLifecycleOwner.get().lifecycle.addObserver(object : DefaultLifecycleObserver {
            override fun onStart(owner: LifecycleOwner) {
                foreground = true
                // In sight: the only time the service may start.
                if (state.value.watch == Watch.NOW) WatchService.start(app)
                poke()
                if (eager) watchMailbox()
            }

            override fun onStop(owner: LifecycleOwner) {
                foreground = false
                // Out of sight the app may stop soon: a change not backed up is now.
                scope.launch { autoBackup() }
            }
        })
        scope.launch {
            // In sight, told on screen; out of sight, money in in a
            // notification: the rest is told as its line finishes.
            events.collect {
                if (!foreground && it.received != null) notifier.payment("event ${noticed.incrementAndGet()}", it.text)
            }
        }
        scope.launch {
            load()
            loop()
        }
        scope.launch {
            // A protection being set up, in sight: its step, how long it
            // has taken and its speed kept current between rounds.
            while (true) {
                delay(OPENING_TICK_MS)
                if (foreground && state.value.contracts.any { it.state == ContractState.Opening }) tickContracts()
            }
        }
    }

    /** The contracts alone read again: cheap enough every few seconds. */
    private fun tickContracts() {
        val contracts = wallet?.let { runCatching { it.contracts() }.getOrNull() } ?: return
        state.update { it.copy(contracts = contracts) }
    }

    /** Keep at work out of sight, or no longer: for the service watching. */
    fun keep(on: Boolean) {
        if (on) keepers.incrementAndGet() else keepers.decrementAndGet()
        poke()
    }

    /**
     * Keep at work out of sight for two rounds in step with Bitcoin, at most
     * `ms`: a look for the periodic job, the app perhaps just started. None
     * with no wallet, or no light client (Tor or the wallet failing).
     */
    suspend fun round(ms: Long) {
        keepers.incrementAndGet()
        try {
            withTimeoutOrNull(ms) {
                started.first { it }
                if (wallet == null || light == null) {
                    AppLog.info("no look out of sight: no wallet or no light client")
                    return@withTimeoutOrNull
                }
                val start = rounds.value
                poke()
                rounds.first { it >= start + 2 }
            }
        } finally {
            keepers.decrementAndGet()
        }
    }

    /** Sync now rather than at the next round. */
    fun poke() {
        wake.trySend(Unit)
    }

    /** Open the wallet and show it, then start the light client. */
    private suspend fun load() {
        mutex.withLock {
            if (walletExists(walletDir)) {
                AppLog.info(
                    (if (prefs.tor) "through Tor" else "Tor off") +
                        if (prefs.torChosen) ", as chosen" else ", by default",
                )
                runCatching { configured(Wallet.open(walletDir, tor())) }
                    .onSuccess { wallet = it }
                    .onFailure { e ->
                        AppLog.warn("opening the wallet", e)
                        state.update { it.copy(problem = explain(e)) }
                    }
            }
        }
        refresh()
        state.update { it.copy(loaded = true) }
        lightLock.withLock { startLight() }
        started.value = true
    }

    /**
     * The app's Tor client, over Tor: made the first time, its state moved
     * out of the wallet's directory where an earlier version kept it. None
     * with Tor off. It lives as long as the app, wallet or not.
     */
    private fun tor(): TorClient? {
        if (!prefs.tor) return null
        torClient?.let { return it }
        synchronized(this) {
            return torClient ?: TorClient(File(files, "tor").path, File(walletDir, "tor").path).also {
                torClient = it
                AppLog.info("Tor's state kept in ${it.dir()}")
            }
        }
    }

    /** The app's Tor client closed, once no wallet reaches anything through it. */
    private fun closeTor() {
        synchronized(this) {
            torClient?.destroy()
            torClient = null
        }
    }

    /**
     * Run the light client of the wallet's network, or the one chosen, wallet
     * or not. Over Tor, everything it reaches goes through the app's Tor
     * client: never directly instead.
     */
    private suspend fun startLight() {
        val network = wallet?.network() ?: prefs.network
        val tor = prefs.tor
        // It holds a password, never logged.
        val proxy = if (!tor) {
            ""
        } else {
            runCatching { tor()?.proxy() }
                .onFailure { e ->
                    AppLog.warn("the light client's way through Tor", e)
                    state.update { it.copy(problem = explain(e)) }
                }
                .getOrNull()
        }
        val key = listOf(chainName(network), prefs.peers, prefs.cdn, proxy ?: "none").joinToString("|")
        if (key == lightOf && light != null) return
        // Between wallet calls: one may be reading through it.
        mutex.withLock { stopLight() }
        if (proxy == null) {
            AppLog.info("the light client cannot reach Bitcoin: no way through Tor")
            return
        }
        val name = chainName(network)
        val dir = File(files, "chain-$name").path
        AppLog.info("starting the light client of $name" + if (tor) ", through Tor" else "")
        runCatching { Mobile.startThrough(name, dir, prefs.peers, prefs.cdn, proxy) }
            .onSuccess {
                light = it
                lightOf = key
                lightAt = SystemClock.elapsedRealtime()
                synchronized(phases) { phases.clear() }
                AppLog.info("light client of $name started")
            }
            .onFailure { e ->
                AppLog.warn("starting the light client", e)
                state.update { it.copy(problem = explain(e)) }
            }
    }

    /**
     * What to tell of a step that needs the wallet's coins on Bitcoin known
     * while the app is behind it, with how far it is: none once caught up.
     */
    private fun stillCatchingUp(): String? {
        val ui = state.value
        if (!behind(ui)) return null
        val step = stages(ui).firstOrNull { it.state != Progress.DONE } ?: return null
        return "Still catching up with Bitcoin (${step.name.lowercase()}: ${step.detail}): " +
            "try again once caught up."
    }

    /** Since when, in sight, the app is behind Bitcoin ([SystemClock.elapsedRealtime]). */
    private var behindSince: Long? = null

    /**
     * Behind Bitcoin in sight for a while, as on a first start or after
     * days away: the service keeps catching up should she leave the app,
     * its progress in its notification. A catch-up of seconds shows none.
     */
    private fun keepCatchingUp() {
        if (!behind(state.value)) {
            behindSince = null
            return
        }
        val now = SystemClock.elapsedRealtime()
        val since = behindSince ?: now.also { behindSince = it }
        if (now - since >= CATCH_UP_AFTER_MS) WatchService.start(app)
    }

    /*
     * Automatic backups. Each field below is read and written under
     * [backupLock] only.
     */

    /** The wallet's files as last seen ([Wallet.backupStamp]), and as they were when last backed up. */
    private var seenStamp: String? = null
    private var backedStamp: String? = null

    /** Since when, and when last, the wallet changed with no backup since ([SystemClock.elapsedRealtime]). */
    private var changedFirst = 0L
    private var changedLast = 0L

    /** When a backup was last looked at, and none tried again before when, after a failure. */
    private var backupLooked = 0L
    private var backupRetry = 0L
    private val backupLock = Mutex()

    /**
     * An automatic backup, if a folder is chosen: the wallet changed, once
     * it rested for 30 seconds, or 5 minutes after a change at the latest,
     * at once out of sight where the app may stop soon; else hourly, which
     * writes one only if the last is a day old or gone from the folder.
     * Rust tells whether a change is worth one.
     */
    private suspend fun autoBackup() {
        val tree = prefs.backupFolder ?: return
        val w = wallet ?: return
        // One at a time: another under way sees this change too.
        if (!backupLock.tryLock()) return
        try {
            val now = SystemClock.elapsedRealtime()
            val stamp = runCatching { w.backupStamp() }.getOrNull() ?: return
            if (stamp != seenStamp) {
                if (seenStamp == backedStamp) changedFirst = now
                seenStamp = stamp
                changedLast = now
            }
            val due = when {
                now < backupRetry -> false
                stamp != backedStamp ->
                    !foreground || now - changedLast >= BACKUP_REST_MS || now - changedFirst >= BACKUP_MOST_MS
                else -> now - backupLooked >= BACKUP_LOOK_MS
            }
            if (due) backupLocked(tree, w, force = false)
        } finally {
            backupLock.unlock()
        }
    }

    /** [backupLocked] under [backupLock], off the main thread. */
    private suspend fun backupNow(tree: String, force: Boolean): Result<Unit> = withContext(Dispatchers.IO) {
        backupLock.withLock {
            val w = wallet ?: return@withLock Result.failure(LicashException.Invalid("there is no wallet yet"))
            backupLocked(tree, w, force)
        }
    }

    /**
     * A backup of `w` written to the folder `tree` if it changed since the
     * last, or if `force`: its files copied from the outbox, those of older
     * ones deleted. Under [backupLock].
     */
    private suspend fun backupLocked(tree: String, w: Wallet, force: Boolean): Result<Unit> = io {
        val now = SystemClock.elapsedRealtime()
        backupLooked = now
        // What this backup holds at least: changes after it are due still.
        val stamp = w.backupStamp()
        val outbox = File(app.noBackupFilesDir, "backup-outbox")
        try {
            val folder = backupDir(Uri.parse(tree), w.backupFolder())
            val present = folder.children().filter { !it.dir }
                .map { FolderFile(it.name, it.bytes.takeIf { b -> b >= 0 }?.toULong()) }
            when (val step = w.prepareBackup(outbox.path, present, force)) {
                // Contract work under way: the change still due.
                is BackupStep.Busy -> return@io
                is BackupStep.Unchanged -> {}
                is BackupStep.Write -> {
                    val run = step.run
                    for (name in run.write) folder.write(name, File(outbox, name))
                    for (name in run.delete) {
                        runCatching { folder.delete(name) }.onFailure { AppLog.warn("an old backup's file", it) }
                    }
                    w.backupPublished(run.generation)
                    prefs.backupAt = System.currentTimeMillis()
                    AppLog.info("backup written: ${run.write.size} files, ${run.delete.size} deleted")
                }
            }
        } finally {
            outbox.listFiles()?.forEach { it.delete() }
        }
        backedStamp = stamp
        backupRetry = 0L
        state.update { it.copy(backup = AutoBackup(prefs.backupAt)) }
    }.onFailure { e ->
        AppLog.warn("an automatic backup", e)
        // Tried again in a while, not every round.
        backupRetry = SystemClock.elapsedRealtime() + BACKUP_RETRY_MS
        state.update { it.copy(backup = AutoBackup(prefs.backupAt, explain(e))) }
    }

    /**
     * The folder the wallet's backups are in: `tree` itself if the user
     * chose that one, else the one of its name in it, made if not there.
     * Backing up and restoring find the same.
     */
    private fun backupDir(tree: Uri, name: String): Folder {
        val root = Folder.of(app, tree)
        return if (root.name() == name) root else root.dir(name)
    }

    /** Automatic backups to the folder `tree` the user chose, the first one written now. */
    suspend fun startBackups(tree: Uri): Result<Unit> {
        val flags = Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION
        val kept = runCatching { app.contentResolver.takePersistableUriPermission(tree, flags) }
        if (kept.isFailure) return Result.failure(kept.exceptionOrNull()!!)
        prefs.backupFolder?.takeIf { it != tree.toString() }?.let { old ->
            runCatching { app.contentResolver.releasePersistableUriPermission(Uri.parse(old), flags) }
        }
        prefs.backupFolder = tree.toString()
        AppLog.info("automatic backups on")
        return backupNow(tree.toString(), force = true)
    }

    /** A backup written now, changed or not. */
    suspend fun backupAgain(): Result<Unit> {
        val tree = prefs.backupFolder ?: return Result.failure(IllegalStateException("no backup folder chosen"))
        return backupNow(tree, force = true)
    }

    /** No more automatic backups: those in the folder stay. */
    fun stopBackups() {
        prefs.backupFolder?.let { old ->
            val flags = Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION
            runCatching { app.contentResolver.releasePersistableUriPermission(Uri.parse(old), flags) }
        }
        prefs.backupFolder = null
        prefs.backupAt = null
        state.update { it.copy(backup = null) }
        AppLog.info("automatic backups off")
    }

    /** A step of catching up logged, once a start, with the time it took since the light client started. */
    private fun phase(step: String) {
        val first = synchronized(phases) { phases.add(step.substringBefore(" to block")) }
        if (first && lightAt > 0) AppLog.info("phase: $step, ${(SystemClock.elapsedRealtime() - lightAt) / 1000} s in")
    }

    private fun stopLight() {
        runCatching { light?.stop() }
        light = null
        lightOf = null
    }

    private suspend fun loop() {
        while (true) {
            if (foreground || keepers.get() > 0) {
                findProviders()
                runCatching { sync() }.onFailure { e ->
                    AppLog.warn("sync", e)
                    state.update { it.copy(problem = explain(e)) }
                }
                prefetchQuotes()
                if (state.value.chain.synced) rounds.update { it + 1 }
                if (foreground) keepCatchingUp()
                autoBackup()
            }
            // Out of sight, what is watched waits for blocks: no hurry,
            // unless catching up.
            val ms = when {
                !foreground && !behind(state.value) -> AWAY_MS
                !foreground -> ROUND_MS
                eager -> EAGER_MS
                else -> ROUND_MS
            }
            withTimeoutOrNull(ms) { wake.receive() }
        }
    }

    private suspend fun sync() {
        val light = light ?: return
        val status = statusOf(light)
        val tor = torStatus()
        state.update { it.copy(chain = status, torStatus = tor) }
        if (tor?.ready == true) phase("Tor ready")
        // Nothing to ask of it before it starts.
        if (status.importing >= 0) return
        var hubs = wallet?.let { local { w -> w.hubs() } } ?: return
        if (hubs.isEmpty()) return
        if (!hubsLearned || System.currentTimeMillis() - hubsLearnedAt > HUBS_MS) {
            learnHubs(hubs)
            hubs = local { it.hubs() }
        }
        val chain = GoChain(light)
        val problems = mutableListOf<String>()
        fun failed(step: String, e: LicashException) {
            AppLog.warn(step, e)
            problems += explain(e)
        }
        if (status.copy(headers = 0, peerHeight = 0) != logged) {
            logged = status.copy(headers = 0, peerHeight = 0)
            AppLog.info(
                "block ${status.height}, ${status.peers} peers, " + if (status.synced) "synced" else "syncing",
            )
        }
        var caughtUp = false
        if (status.synced) {
            var before: UInt? = null
            while (true) {
                val scan = try {
                    locked { it.scan(chain) }
                } catch (e: LicashException) {
                    failed("looking for payments on Bitcoin", e)
                    // A hub may serve other blocks now: learn it next round.
                    hubsLearned = false
                    break
                }
                state.update { it.copy(scan = scan) }
                if (scan.found > 0u) {
                    AppLog.info("found ${scan.found} coins of ${scan.foundValue} sat to block ${scan.next - 1u}")
                }
                if (scan.next > scan.height) {
                    caughtUp = true
                    phase("payments looked for to block ${scan.height}")
                    break
                }
                // The hubs' tweaks lag the chain: on next round.
                if (scan.next == before) break
                before = scan.next
                AppLog.info("looked in to block ${scan.next - 1u} of ${scan.height}")
            }
        }
        // Not while looking in blocks again: what it forgot would show as
        // on its way until found again.
        if (caughtUp || !status.synced) {
            try {
                val incoming = locked { it.incoming(chain) }
                state.update { it.copy(incoming = incoming) }
            } catch (e: LicashException) {
                failed("payments on their way", e)
            }
        }
        try {
            locked { it.resume() }
        } catch (e: LicashException) {
            failed("finishing what is under way", e)
        }
        // With the chain as it is: a claim waits for the provider's coin.
        if (status.synced) {
            try {
                locked { it.followPayouts(chain) }
            } catch (e: LicashException) {
                failed("paying out on Bitcoin", e)
            }
            resumeContracts(chain)
            // Its balance looked for, and a withdrawal made if due.
            try {
                locked { it.runCold(chain) }
            } catch (e: LicashException) {
                AppLog.info("cold storage deferred: ${explain(e)}")
            }
        }
        if (hubs.any { it.mailbox }) {
            try {
                collect()
            } catch (e: LicashException) {
                failed("the mailbox", e)
            }
        }
        // Only with every coin found: a partial look could move part.
        if (caughtUp) {
            try {
                offer()
            } catch (e: LicashException) {
                failed("looking for protection to offer", e)
            }
        }
        // Common SDK guards handle privacy rest, fee ceilings and the
        // daily budget. Failure here does not fail completed sync work.
        try {
            locked { it.maintainNotes() }
        } catch (e: LicashException) {
            AppLog.info("note maintenance deferred: ${explain(e)}")
        }
        state.update { it.copy(problem = problems.firstOrNull()) }
        refresh()
        tell()
    }

    /**
     * Take the payments in the mailbox, each told of at once, their notes
     * unlinked after: whether there were any.
     */
    private suspend fun collect(): Boolean {
        val got = locked { it.collectNow() }
        for (one in got) {
            AppLog.info("received ${one.amount} sat in the mailbox")
            notices.tryEmit(Event("Received ${sat(one.amount)}", one.amount))
        }
        return got.isNotEmpty()
    }

    /**
     * Look in the mailbox every [MAIL_MS] while someone waits to be paid, in
     * sight, apart from the rounds and what they wait on.
     */
    private fun watchMailbox() {
        if (!watchingMailbox.compareAndSet(false, true)) return
        scope.launch {
            try {
                while (eager && foreground) {
                    val hubs = wallet?.let { runCatching { local { w -> w.hubs() } }.getOrNull() }
                    if (hubs != null && hubs.any { it.mailbox }) {
                        try {
                            if (collect()) refresh()
                        } catch (e: LicashException) {
                            AppLog.warn("the mailbox, looked in apart", e)
                        }
                    }
                    delay(MAIL_MS)
                }
            } finally {
                watchingMailbox.set(false)
            }
            // Asked again as it stopped.
            if (eager && foreground) watchMailbox()
        }
    }

    /** Learn again what each hub trusted serves: it may have changed since added. */
    private suspend fun learnHubs(hubs: List<Hub>) {
        for (hub in hubs) {
            try {
                locked { it.addHub(hub.url) }
            } catch (e: LicashException) {
                AppLog.warn("learning what ${hub.url} serves", e)
            }
        }
        hubsLearned = true
        hubsLearnedAt = System.currentTimeMillis()
    }

    /**
     * Ask each hub trusted that opens contracts for its quote ahead, in
     * the background, once the last is older than [QUOTE_MS] or what is
     * on Bitcoin changed: protecting shows at once ([contractQuote]).
     */
    private fun prefetchQuotes() {
        if (!foreground || !state.value.chain.synced || state.value.opening != null) return
        if (!quoting.compareAndSet(false, true)) return
        scope.launch {
            try {
                val onchain = local { it.balance() }.onchain
                val now = System.currentTimeMillis()
                for (hub in local { it.hubs() }.filter { it.contracts != null }) {
                    val cached = quotes[hub.url]
                    if (cached != null && cached.second == onchain && now - cached.third < QUOTE_MS / 2) continue
                    runCatching { local { it.contractQuote(hub.url) } }
                        .onSuccess { quotes[hub.url] = Triple(it, onchain, System.currentTimeMillis()) }
                        .onFailure { AppLog.info("asking ${hub.url} for a quote ahead: ${it.message}") }
                }
            } catch (e: LicashException) {
                AppLog.warn("asking for quotes ahead", e)
            } finally {
                quoting.set(false)
            }
        }
    }

    /**
     * Look for a contract to offer for what was paid on Bitcoin, to protect
     * it once in the balance ([Ui.offer]): again once what is there
     * changes, or every [OFFER_MS]. None for what was cashed out to this
     * wallet, which stays, nor while one is being opened.
     */
    private suspend fun offer() {
        val balance = local { it.balance() }
        if (balance.onchain <= balance.kept || state.value.opening != null) {
            state.update { it.copy(offer = null) }
            return
        }
        val now = System.currentTimeMillis()
        if (balance.onchain == offeredFor && now - offeredAt < OFFER_MS) return
        val offer = try {
            local { it.contractOffer() }
        } catch (e: LicashException) {
            AppLog.info("no contract to offer now: ${explain(e)}")
            return
        }
        offeredFor = balance.onchain
        offeredAt = now
        AppLog.info(offer?.let { "offering a contract at ${it.hub} of ${it.suggestedCap} sat" } ?: "no contract to offer")
        state.update { it.copy(offer = offer, offerDeclined = offerDeclined(offer, prefs.offerDeclined)) }
    }

    /** Not to offer `q` again unasked: its coin's offers are declined. */
    fun declineOffer(q: ContractQuote) {
        prefs.offerDeclined = offeredCoin(q) ?: return
        state.update { it.copy(offerDeclined = true) }
    }

    /**
     * Look for the swap providers announced, apart from the rounds, for a
     * quote to ask them without waiting on relays: every [PROVIDERS_MS]
     * once looked for, [PROVIDERS_RETRY_MS] after failing to; at once for
     * a wallet opened since.
     */
    private fun findProviders() {
        val wallet = wallet ?: return
        val due = providersDue
        if (due != null && due.first === wallet && System.nanoTime() < due.second) return
        if (!findingProviders.compareAndSet(false, true)) return
        scope.launch {
            val wait = runCatching { wallet.refreshProviders() }
                .onFailure { AppLog.warn("looking for swap providers", it) }
                .fold({ PROVIDERS_MS }, { PROVIDERS_RETRY_MS })
            providersDue = wallet to System.nanoTime() + wait * 1_000_000
            findingProviders.set(false)
        }
    }

    /**
     * Carry on with the contracts being opened, watch the funding of those
     * signed and carry on those forced closed, in the background: an
     * opening carried on downloads for minutes. Nothing to do once each is
     * confirmed or given up, and each forced closed over.
     */
    private fun resumeContracts(chain: GoChain) {
        val ui = state.value
        val waiting = ui.contracts.any { carriedOn(it) } || ui.recoveries.any { carriedOn(it) }
        val protecting = ui.contracts.any { it.state == ContractState.Protecting }
        if (!(waiting || protecting) || !contracting.compareAndSet(false, true)) return
        scope.launch {
            try {
                if (waiting) local { it.resumeContracts(chain) }
            } catch (e: Exception) {
                // Not to bring the app down from a coroutine of its own.
                AppLog.warn("carrying on with contracts", e)
            }
            try {
                // As she consented, else only told: nothing done without.
                if (protecting) {
                    val margin = exitMarginBlocks(prefs.exitMarginDays)
                    val exits = local { it.driveExits(chain, margin) }
                    for (e in exits) e.error?.let { AppLog.warn("exit of contract ${e.id}, ${e.step}: $it") }
                    state.update { it.copy(exits = exits) }
                }
            } catch (e: Exception) {
                AppLog.warn("exiting contracts ending", e)
            } finally {
                contracting.set(false)
            }
            refresh()
        }
    }

    /**
     * Tell of coins found on Bitcoin and payments on their way not told of
     * yet: none of those there when the app started, nor coins found again
     * after looking in blocks again.
     */
    private fun tell() {
        val ui = state.value
        val coins = ui.history.filter { it.kind == Kind.ONCHAIN }
        val told = told ?: (coins + ui.incoming).map { it.id }.toMutableSet().also { told = it }
        val arrived = coins.filter { told.add(it.id) }.sumOf { it.amount }
        if (arrived > 0uL) notices.tryEmit(Event("${sat(arrived)} arrived on Bitcoin", arrived))
        val coming = ui.incoming.filter { told.add(it.id) }.sumOf { it.amount }
        if (coming > 0uL) notices.tryEmit(Event("${sat(coming)} on its way on Bitcoin"))
    }

    private fun refresh() {
        val shown = runCatching {
            wallet?.let {
                val contracts = runCatching { it.contracts() }
                    .onFailure { e -> AppLog.warn("reading the contracts", e) }
                    .getOrDefault(state.value.contracts)
                val recoveries = runCatching { it.recoveries() }
                    .onFailure { e -> AppLog.warn("reading the forced closes", e) }
                    .getOrDefault(state.value.recoveries)
                val history = it.history()
                // From the wallet alone, no network: Home tells each step.
                val under = detailsToRead(history)
                    .mapNotNull { a -> runCatching { it.details(a.kind, a.id) }.getOrNull()?.let { d -> a.key to d } }
                    .toMap()
                val raised = runCatching { it.feesRaised() }
                    .onFailure { e -> AppLog.warn("reading the fees agreed", e) }
                    .getOrDefault(state.value.raised)
                val noteMaintenance = runCatching { it.noteMaintenanceNeeded() }
                    .onFailure { e -> AppLog.warn("reading future note work", e) }
                    .getOrDefault(state.value.noteMaintenance)
                val onTheirWay = runCatching { it.onTheirWay() }
                    .onFailure { e -> AppLog.warn("reading the payments on their way", e) }
                    .getOrDefault(state.value.onTheirWay)
                val cold = runCatching { it.cold() }
                    .onFailure { e -> AppLog.warn("reading cold storage", e) }
                    .getOrDefault(state.value.cold)
                Shown(
                    it.network(), it.balance(), history, it.hubs(), raised, it.tor(), contracts, recoveries, under,
                    noteMaintenance, onTheirWay, cold,
                )
            }
        }.onFailure { AppLog.warn("reading the wallet", it) }.getOrNull()
        state.update {
            when {
                wallet == null -> it.copy(
                    wallet = false,
                    noteMaintenance = false,
                    network = prefs.network,
                    developer = prefs.developer,
                    tor = prefs.tor,
                )
                shown == null -> it
                else -> it.copy(
                    wallet = true,
                    network = shown.network,
                    developer = prefs.developer,
                    balance = shown.balance,
                    history = shown.history,
                    hubs = shown.hubs,
                    raised = shown.raised,
                    tor = shown.tor,
                    contracts = shown.contracts,
                    recoveries = shown.recoveries,
                    under = shown.under,
                    noteMaintenance = shown.noteMaintenance,
                    asked = prefs.askedAt,
                    onTheirWay = shown.onTheirWay,
                    cold = shown.cold,
                    backup = prefs.backupFolder?.let { _ -> it.backup ?: AutoBackup(prefs.backupAt) },
                )
            }
        }
        if (shown != null) watch()
    }

    /**
     * After each look at the wallet: out of sight, tell of the payments
     * finished and of payouts nearing their deadline; and keep watching,
     * the service started while in sight, as long as anything is under way.
     */
    @Synchronized
    private fun watch() {
        val ui = state.value
        val level = watchOf(ui, System.currentTimeMillis() / 1000)
        state.update { it.copy(watch = level) }
        val look = look(ui.history, prefs.watched, seen)
        prefs.watched = look.under
        seen = ui.history.map { it.key }.toSet()
        val near = nearing(ui)
        var warned = prefs.warned intersect look.under
        if (!foreground) {
            for (activity in look.finished) news(activity)?.let { notifier.payment(activity.key, it) }
            for ((key, by) in near) {
                if (key in warned) continue
                notifier.deadline(
                    key,
                    "A payment on Bitcoin must be claimed by block ${digits(by.toULong())}, or it fails. " +
                        "Licash claims it once open with a connection.",
                )
                warned = warned + key
            }
        }
        prefs.warned = warned
        // In sight or not: nothing else shows them. Each told once.
        var told = prefs.exitsTold
        for (notice in exitNotices(ui.exits, ui.recoveries)) {
            if (notice.key in told) continue
            if (notice.urgent) notifier.deadline(notice.key, notice.text) else notifier.payment(notice.key, notice.text)
            told = told + notice.key
        }
        prefs.exitsTold = told
        if (level == Watch.NOW && foreground) WatchService.start(app)
        val periodic = level != Watch.NONE
        if (periodic != looking) {
            looking = periodic
            if (periodic) WatchJob.schedule(app) else WatchJob.cancel(app)
        }
    }

    private data class Shown(
        val network: Network,
        val balance: Balance,
        val history: List<Activity>,
        val hubs: List<Hub>,
        val raised: List<FeesRaised>,
        val tor: Boolean,
        val contracts: List<Contract>,
        val recoveries: List<Recovery>,
        val under: Map<String, Details>,
        val noteMaintenance: Boolean,
        val onTheirWay: UInt,
        val cold: ColdStorage?,
    )

    /** A new wallet of the network chosen, born at the chain's height if known. */
    suspend fun create(): Result<Unit> = io {
        mutex.withLock {
            val light = light
            val birthday = if (light != null && light.synced()) light.height().toUInt() else 0u
            wallet = configured(Wallet.create(walletDir, prefs.network, birthday, tor()))
            AppLog.info("wallet made at block $birthday")
        }
        // Running since the app started, of this network: shown at once,
        // not waiting on it.
        scope.launch { lightLock.withLock { startLight() } }
        refresh()
        poke()
    }

    /** The 24 words restoring this wallet's keys: secret, shown only when asked, never logged. */
    suspend fun backupPhrase(): Result<List<String>> = io { local { it.backupPhrase() }.split(" ") }

    /**
     * Everything this wallet holds as it is now, saved `to` a file the user
     * chose: unencrypted, private. Written whole to the cache first, so a
     * failure leaves no half a backup behind as if it were one.
     */
    suspend fun exportBackup(to: Uri): Result<Unit> = io {
        val made = File(app.cacheDir, "backup-${System.nanoTime()}.licash")
        try {
            locked { it.exportStateBundle(made.path) }
            val out = app.contentResolver.openOutputStream(to, "wt") ?: error("that file cannot be written")
            out.use { o -> made.inputStream().use { it.copyTo(o) } }
            AppLog.info("full backup saved, ${made.length()} bytes")
        } finally {
            made.delete()
        }
    }

    /**
     * A wallet of the network chosen from its 24 words: its keys, and its
     * coins once Bitcoin is looked through from `birthday`, the block
     * written down with them, or from the first block if none.
     */
    suspend fun restorePhrase(words: List<String>, birthday: UInt?): Result<Unit> = restored {
        Wallet.fromPhrase(walletDir, words.joinToString(" "), prefs.network, birthday, tor())
    }

    /** The block the wallet's coins on Bitcoin are looked for from, once known. */
    suspend fun birthday(): Result<UInt?> = io { local { it.birthday() } }

    /** How the light client and Tor stand now, for a screen showing them: not waiting for the next round. */
    suspend fun live() {
        val light = light
        val chain = withContext(Dispatchers.IO) { light?.let { runCatching { statusOf(it) }.getOrNull() } }
        val tor = withContext(Dispatchers.IO) { torStatus() }
        state.update { it.copy(chain = chain ?: it.chain, torStatus = tor) }
    }

    /** How the app's Tor client stands, over Tor; it starts connecting if it is not. */
    private fun torStatus(): TorStatus? = runCatching { tor()?.status() }.getOrNull()

    /**
     * Start Tor afresh, as asked: what it learned of the Tor network (its
     * state and directory cache) forgotten, the wallet opened again and
     * the light client started again through it. Nothing the wallet holds
     * is touched.
     */
    suspend fun resetTor(): Result<Unit> = io {
        lightLock.withLock {
            mutex.withLock {
                // The light client and the wallet reach everything through Tor.
                stopLight()
                wallet?.destroy()
                wallet = null
                closeTor()
                try {
                    // Where it is kept, and where an earlier version kept it.
                    for (dir in listOf(File(files, "tor"), File(walletDir, "tor"))) {
                        for (part in listOf("state", "cache")) File(dir, part).deleteRecursively()
                    }
                    AppLog.info("Tor's state forgotten")
                } finally {
                    if (walletExists(walletDir)) wallet = configured(Wallet.open(walletDir, tor()))
                }
            }
            state.update { it.copy(torStatus = null) }
            startLight()
        }
        refresh()
        poke()
    }

    /** The wallet a full backup `from` holds, of its own network. Never over a wallet there. */
    suspend fun restoreBackup(from: Uri): Result<Unit> {
        val copy = File(app.cacheDir, "restore-${System.nanoTime()}.licash")
        return try {
            io {
                val input = app.contentResolver.openInputStream(from) ?: error("that file cannot be read")
                input.use { i -> copy.outputStream().use { i.copyTo(it) } }
            }.fold(
                { restored { Wallet.fromStateBundle(walletDir, copy.path, tor()) } },
                { Result.failure(it) },
            )
        } finally {
            copy.delete()
        }
    }

    /** Where a backup folder's files are copied to, to restore from. */
    private val restoreCopy get() = File(app.noBackupFilesDir, "restore-folder")

    /**
     * The newest backup of the wallet of `words`, on the network chosen, in
     * the folder `tree` or in its own folder in it: none if none restores.
     * Its files are copied to the phone, kept for [restoreFolder].
     */
    suspend fun findFolderBackup(tree: Uri, words: List<String>): Result<BackupFound?> = io {
        val phrase = words.joinToString(" ")
        val name = backupFolderName(phrase, prefs.network)
        val root = Folder.of(app, tree)
        // As [backupDir] finds it, never made here.
        val folder = if (root.name() == name) root else root.find(name)
        val copy = restoreCopy.apply { deleteRecursively(); mkdirs() }
        if (folder == null) return@io null
        for (c in folder.children()) {
            if (!c.dir && c.name.endsWith(".lcb")) folder.read(c, File(copy, c.name))
        }
        findBackup(phrase, prefs.network, copy.path)
    }

    /**
     * The wallet of `words` from its backup of `generation` found by
     * [findFolderBackup], its automatic backups going on to `tree`.
     */
    suspend fun restoreFolder(tree: Uri, words: List<String>, generation: ULong): Result<Unit> {
        val phrase = words.joinToString(" ")
        val done = restored { Wallet.fromBackup(walletDir, phrase, prefs.network, restoreCopy.path, generation, tor()) }
        restoreCopy.deleteRecursively()
        if (done.isSuccess) startBackups(tree).onFailure { AppLog.warn("backups after a restore", it) }
        return done
    }

    /**
     * The wallet `open` makes, there being none: its network the app's from
     * now on, and Tor as that network has it unless chosen.
     */
    private suspend fun restored(open: () -> Wallet): Result<Unit> = io {
        mutex.withLock {
            check(wallet == null && !walletExists(walletDir)) { "a wallet is here already" }
            var w = open()
            prefs.network = w.network()
            if (w.tor() != prefs.tor) {
                w.destroy()
                w = Wallet.open(walletDir, tor())
            }
            wallet = configured(w)
            hubsLearned = false
            AppLog.info("wallet restored, of ${chainName(prefs.network)}")
        }
        lightLock.withLock { startLight() }
        refresh()
        poke()
    }

    /**
     * Look through Bitcoin again, as asked: the wallet from its first block,
     * the light client's blocks fetched afresh. Notes, contracts and what is
     * under way are untouched. Refused, nothing changed, if the wallet's
     * first block cannot be known yet.
     */
    suspend fun resetChain(): Result<Unit> = io {
        lightLock.withLock {
            mutex.withLock {
                wallet?.let { w ->
                    val from = w.rescanChain(light?.let { GoChain(it) } ?: NoChain)
                    AppLog.info("looking through Bitcoin again" + (from?.let { " from block $it" } ?: ""))
                }
                stopLight()
            }
            val dir = File(files, "chain-${chainName(wallet?.network() ?: prefs.network)}")
            dir.deleteRecursively()
            AppLog.info("light client's blocks forgotten")
            state.update { it.copy(chain = ChainStatus(), scan = null) }
            startLight()
        }
        refresh()
        poke()
    }

    suspend fun addHub(url: String): Result<Hub> = io {
        val hub = locked { it.addHub(url) }
        refresh()
        poke()
        hub
    }

    /**
     * What the hub at [hub] charges beyond what was agreed, as the wallet
     * last heard: none if no more. A payment refused for it asked anew.
     */
    suspend fun feesRaised(hub: String): Result<FeesRaised?> =
        io { local { w -> w.feesRaised().find { it.hub == hub } } }

    /** Agree to pay a hub what [raised] shows it charges, and no more. */
    suspend fun approveFees(raised: FeesRaised): Result<Unit> = io {
        locked { it.approveFees(raised.hub, raised.charged) }
        refresh()
    }

    suspend fun optimizationQuote(hub: String): Result<NoteOptimization?> =
        io { local { it.optimizationQuote(hub) } }

    /** The shown fee is a hard cap, including if inventory changed since. */
    suspend fun optimizeNotes(quote: NoteOptimization): Result<NoteOptimization?> = io {
        try {
            locked { it.optimizeNotes(quote.hub, quote.fee) }
        } finally {
            refresh()
        }
    }

    suspend fun code(amount: ULong?): Result<Code> = io { local { it.paymentCode(amount) } }

    /** The code on Bitcoin: an address any wallet pays, handed out, and watched from then on. */
    suspend fun bitcoinCode(amount: ULong?): Result<Code> = io { local { it.bitcoinCode(amount) } }

    /** The silent payment address alone. */
    suspend fun silentAddress(): Result<String> = io { local { it.silentAddress() } }

    /**
     * The wallet's static address of donations, through a swap provider
     * announced that takes them at a hub of its code: null if none does.
     */
    suspend fun donationAddress(): Result<DonationAddress?> = io { local { it.donationAddress() } }

    /** Keep the provider of the donation address `lnurl` shown: it shows from then on. */
    suspend fun keepDonationAddress(lnurl: String): Result<Unit> = io { local { it.keepDonationAddress(lnurl) } }

    /** More of a line of the history, as far as the wallet looked. */
    suspend fun details(activity: Activity): Result<Details> =
        io { local { it.details(activity.kind, activity.id) } }

    /** The notes a line of the history spent, and those it made the wallet's. */
    suspend fun notesOf(activity: Activity): Result<NotesOf> =
        io { local { it.notesOf(activity.kind, activity.id) } }

    /** Where to see the transaction `txid`, if anywhere. */
    fun explorerLink(txid: String): String? {
        return explorerBase()?.let { "$it/tx/$txid" }
    }

    fun explorerAddress(address: String): String? = explorerBase()?.let { "$it/address/$address" }

    private fun explorerBase(): String? {
        val base = when (val chosen = prefs.explorer) {
            "default" -> explorer(state.value.network)
            "" -> null
            else -> chosen
        }
        return base?.trimEnd('/')
    }

    suspend fun scanned(text: String): Result<Scanned> = io { local { it.scanned(text) } }

    /**
     * How paying `amount`, or what the code `code` asks, in notes goes, and
     * all it costs: [pay] pays it so. At a hub not trusted, the payee's,
     * visited and proving it is the one named, if no hub trusted is.
     */
    /** What paying `code` costs now at a hub trusted holding enough, asking nothing of the network: none if no such hub. */
    suspend fun payFee(code: String, amount: ULong?): Result<ULong?> =
        io { local { it.payFee(code, amount) } }

    suspend fun payQuote(code: String, amount: ULong?): Result<PayQuote> =
        io { local { it.payQuote(code, amount) } }

    /** Every way of paying `code`: through each swap provider that could. */
    suspend fun payOffers(code: String, amount: ULong?): Result<List<PayOffer>> =
        io { local { it.payOffers(code, amount) } }

    /** Quote paying `code` through the provider at `provider`: paid so next. */
    suspend fun choosePayOffer(code: String, amount: ULong?, provider: String): Result<PayQuote> =
        io { local { it.choosePayOffer(code, amount, provider) } }

    /**
     * Pay `code` `amount`, or what it asks: answered once its hub made the
     * payment, sent to the payee's mailbox right after, in the background.
     */
    suspend fun pay(code: String, amount: ULong?): Result<Paid> = io {
        val asked = System.nanoTime()
        val paid = mutex.withLock {
            val waited = (System.nanoTime() - asked) / 1_000_000
            // What a payment waits for before it starts: a round under way.
            if (waited >= 500) AppLog.info("paying waited $waited ms for the wallet")
            local { it.payNow(code, amount) }
        }
        AppLog.info("paid in ${(System.nanoTime() - asked) / 1_000_000} ms")
        refresh()
        paid
    }

    /**
     * What paying the Lightning invoice `text` costs the cheapest way: through
     * a hub's gateway, or through a swap provider.
     */
    suspend fun invoiceQuote(text: String): Result<InvoiceQuote> = io { local { it.invoiceQuote(text) } }

    /** Every way to pay the Lightning invoice `text`, the cheapest first. */
    suspend fun invoiceOffers(text: String): Result<List<InvoiceQuote>> = io { local { it.invoiceOffers(text) } }

    /**
     * Pay the Lightning invoice `text` as `quote` says: through its hub's
     * gateway, or through its provider for a lock of notes at its hub, for
     * no more than the provider quoted. Done, or under way.
     */
    suspend fun payInvoice(text: String, quote: InvoiceQuote): Result<Activity> = io {
        val provider = quote.provider
        val paid = locked {
            if (provider == null) it.payInvoiceWithin(text, quote.hub, quote.fee)
            else it.payInvoiceVia(text, quote.hub, provider.url, provider.fee)
        }
        refresh()
        poke()
        paid
    }

    /**
     * Ask the hub taking it for least for a Lightning invoice asking the
     * payer `amount`, its fee in it: the notes it pays the wallet, less.
     */
    suspend fun askInvoiceFor(amount: ULong): Result<LightningInvoice> = io {
        val asked = locked { it.askInvoiceFor(amount) }
        // Perhaps paid out of sight: looked for now and then for a while.
        prefs.askedAt = System.currentTimeMillis() / 1000
        refresh()
        asked
    }

    /** The contacts, by name. */
    suspend fun contacts(): Result<List<Contact>> = io { local { it.contacts() } }

    /** Keep the payment code `code` as `name`'s: refused for a name another contact has. */
    suspend fun saveContact(name: String, code: String): Result<Contact> = io { local { it.saveContact(name, code) } }

    /** Forget the contact named `name`. */
    suspend fun removeContact(name: String): Result<Unit> = io { local { it.removeContact(name) } }

    /** The contact the payment code `code` pays, if one does. */
    suspend fun contactOf(code: String): Result<Contact?> = io { local { it.contactOf(code) } }

    /** Where the invoice of `hash` stands: its notes taken once it is paid. */
    suspend fun followInvoice(hash: String): Result<LightningInvoice> = io {
        val followed = locked { it.followInvoice(hash) }
        if (followed.state != State.PENDING) refresh()
        followed
    }

    /**
     * What paying `amount` to the Bitcoin address `address` costs through a
     * swap provider, from notes at `hub` or the hub holding the most: all
     * the notes there pay out if `amount` is null.
     */
    suspend fun payoutQuote(address: String, amount: ULong?, hub: String? = null): Result<PayoutQuote> =
        io { local { it.payoutQuote(address, amount, hub) } }

    /** The quotes of [payoutQuote] through every provider that pays it out. */
    suspend fun payoutOffers(address: String, amount: ULong?, hub: String?): Result<List<PayoutQuote>> =
        io { local { it.payoutOffers(address, amount, hub) } }

    /** An address on Bitcoin of this wallet's, to cash out to. */
    suspend fun bitcoinAddress(): Result<String> = io { local { it.bitcoinAddress() } }

    /** Move the largest coin on Bitcoin, whole and alone, into Licash now, one cashed out here too. */
    suspend fun moveLargest(): Result<Activity> = io {
        val light = light ?: throw LicashException.Unavailable("not connected to Bitcoin yet")
        stillCatchingUp()?.let { throw LicashException.Unavailable(it) }
        val moving = locked { it.moveLargestToLicash(GoChain(light)) }
        refresh()
        poke()
        moving
    }

    /**
     * Pay out as `quote` says: under way until its claim confirms. If its
     * provider fails before the notes are locked to it, through another
     * paying `asked` (all if null) for no more in fees.
     */
    suspend fun payOut(quote: PayoutQuote, asked: ULong?): Result<Activity> = io {
        val paying = locked { it.payOutOrAnother(quote, asked) }
        refresh()
        poke()
        paying
    }

    /**
     * What moving `amount` of the notes at hub `from` to hub `to` costs
     * through each swap provider found, as preferred: all of them if null.
     */
    suspend fun moveOffers(from: String, to: String, amount: ULong?): Result<List<MoveQuote>> =
        io { local { it.moveOffers(from, to, amount) } }

    /**
     * Move as the first of `offers` says; through the next costing no more
     * if its provider fails before the notes are locked to it.
     */
    suspend fun moveNotes(offers: List<MoveQuote>): Result<Activity> = io {
        val moving = locked { it.moveNotes(offers) }
        refresh()
        poke()
        moving
    }

    /** What opening a recovery contract at `hub` costs now, and the coin that would pay it. */
    /**
     * `hub`'s contract quote: the one asked ahead if fresh ([prefetchQuotes]),
     * at once, else asked now.
     */
    suspend fun contractQuote(hub: String): Result<ContractQuote> = io {
        val onchain = local { it.balance() }.onchain
        val cached = quotes[hub]
        if (cached != null && cached.second == onchain && System.currentTimeMillis() - cached.third < QUOTE_MS) {
            cached.first
        } else {
            local { it.contractQuote(hub) }.also { quotes[hub] = Triple(it, onchain, System.currentTimeMillis()) }
        }
    }

    suspend fun contractInspection(id: String): Result<org.licash.core.ContractInspection> = io { local { it.contractInspection(id) } }

    /**
     * The least a coin of hers on Bitcoin should hold before she forces a
     * contract closed, for a challenge's fee, if she holds none: null if she
     * does, or while not connected to Bitcoin.
     */
    suspend fun challengeShortfall(): Result<ULong?> = io {
        val light = light ?: return@io null
        local { it.challengeShortfall(GoChain(light)) }
    }

    /** Planning from this wallet's signed terms; never starts a close. */
    suspend fun contractSafety(id: String): Result<org.licash.core.ContractSafety> = io {
        local { it.contractSafety(id, exitMarginBlocks(prefs.exitMarginDays), 6u) }
    }

    /**
     * Open a recovery contract at `hub` for at most `maxPrice`, of the
     * terms `chosen` within the hub's range if any, else of its fixed
     * ones, in the background: it takes minutes, its downloads large.
     * Told once signed, or why not. One at a time.
     */
    fun openContract(hub: String, maxPrice: ULong, chosen: Chosen? = null) {
        val light = light
        if (light == null) {
            notices.tryEmit(Event("Not connected to Bitcoin yet: try again in a moment."))
            return
        }
        stillCatchingUp()?.let {
            notices.tryEmit(Event(it))
            return
        }
        var started = false
        state.update {
            started = it.opening == null
            if (started) it.copy(opening = hub, offer = null) else it
        }
        if (!started) return
        AppLog.info("opening a contract at $hub${chosen?.let { " of ${it.cap} sat over ${it.blocks} blocks" } ?: ""}")
        scope.launch {
            try {
                val chain = GoChain(light)
                val opened = local {
                    if (chosen == null) {
                        it.openContract(chain, hub, null, maxPrice)
                    } else {
                        it.openChosenContract(
                            chain, hub, chosen.cap, chosen.blocks, maxPrice, chosen.coin,
                        )
                    }
                }
                AppLog.info("contract ${opened.id} signed at $hub, funded by ${opened.funding}")
                notices.tryEmit(Event("Protection set up at ${host(hub)}: on once confirmed"))
            } catch (e: Exception) {
                AppLog.warn("opening a contract at $hub", e)
                notices.tryEmit(Event("Protection not set up. ${explain(e)}"))
            } finally {
                // What is left on Bitcoin is offered for afresh.
                offeredFor = null
                state.update { it.copy(opening = null) }
                refresh()
                poke()
            }
        }
    }

    /**
     * Force contract `id` closed, its hub silent, in the background: the
     * notes at its hub, up to its cap, leave the balance, claimed on
     * Bitcoin. Told once claimed, or why not.
     */
    fun forceClose(id: String) {
        val light = light
        if (light == null) {
            notices.tryEmit(Event("Not connected to Bitcoin yet: try again in a moment."))
            return
        }
        AppLog.info("forcing contract $id closed")
        scope.launch {
            try {
                val forced = local { it.forceCloseContract(GoChain(light), id) }
                AppLog.info("contract $id forced closed: ${forced.stage}, ${forced.claimed} sat claimed")
                notices.tryEmit(Event("Forcing closed: ${sat(forced.claimed)} claimed on Bitcoin"))
            } catch (e: Exception) {
                AppLog.warn("forcing contract $id closed", e)
                notices.tryEmit(Event("Not forced closed. ${explain(e)}"))
            } finally {
                refresh()
                poke()
            }
        }
    }

    /**
     * Close contract `id` early with its hub, in the background: what it
     * holds at the hub, up to its cap, and its reserve, back on Bitcoin
     * in the hub's transaction. Told once it is signed, or why not.
     */
    fun closeContract(id: String) {
        val light = light
        if (light == null) {
            notices.tryEmit(Event("Not connected to Bitcoin yet: try again in a moment."))
            return
        }
        AppLog.info("closing contract $id early")
        scope.launch {
            try {
                val fold = local { it.closeContract(GoChain(light), id) }
                AppLog.info("contract $id closed early by $fold")
                notices.tryEmit(Event("Closed with the hub: your notes and reserve come back on Bitcoin once its transaction confirms"))
            } catch (e: Exception) {
                AppLog.warn("closing contract $id early", e)
                notices.tryEmit(Event("Not closed. ${explain(e)}"))
            } finally {
                refresh()
                poke()
            }
        }
    }

    /** Let forcing contract `id` closed pay up to `sats` in all to publish its claim, then carry it on. */
    suspend fun allowPublicationFee(id: String, sats: ULong): Result<Unit> = io {
        local { it.allowPublicationFee(id, sats) }
        AppLog.info("forcing contract $id closed may pay $sats sat to publish")
        refresh()
        poke()
    }

    /** Give up forcing contract `id` closed, its notes back: only until its request is final. */
    suspend fun giveUpRecovery(id: String): Result<Unit> = io {
        local { it.giveUpRecovery(id) }
        AppLog.info("forcing contract $id closed given up")
        refresh()
    }

    /**
     * The hubs announced on the relays its hubs name and on the default
     * ones, not used yet, and how many more did not answer.
     */
    suspend fun findHubs(): Result<HubsFound> = io {
        local { it.findHubs(defaultRelays(it.network(), it.tor())) }
    }

    /** What the wallet holds at each hub it trusts. */
    suspend fun holdings(): Result<List<Holding>> = io { local { it.holdings() } }

    /** What it holds at hubs it only visited, to pay a payee there. */
    suspend fun visited(): Result<List<Holding>> = io { local { it.visited() } }

    /** How many notes of each denomination it holds, at each hub. */
    suspend fun notes(): Result<List<Notes>> = io { local { it.notes() } }

    /** Take a payment handed over, trusting its hub if `trust`. */
    suspend fun receive(payment: String, trust: Boolean): Result<Activity> = io {
        val got = locked { it.receive(payment, trust) }
        refresh()
        got
    }

    /**
     * Reach hubs, relays and Bitcoin through Tor, or not: the wallet opens
     * again, and the light client starts again its new way.
     */
    suspend fun setTor(on: Boolean): Result<Unit> = io {
        try {
            mutex.withLock {
                prefs.tor = on
                wallet?.let {
                    it.destroy()
                    wallet = null
                    wallet = configured(Wallet.open(walletDir, tor()))
                }
            }
        } finally {
            lightLock.withLock {
                startLight()
                // Off: nothing reaches anything through it any more.
                if (!on) closeTor()
            }
        }
        refresh()
    }

    /** Renew notes a restarted hub no longer unlinks for at most `ppm` parts per million of them. */
    suspend fun setRenewLimit(ppm: UInt): Result<Unit> = io {
        mutex.withLock {
            prefs.renewPpm = ppm
            wallet?.setRenewLimit(ppm)
        }
        Unit
    }

    /** The most the wallet pays to renew notes, in parts per million, if there is a wallet. */
    fun renewLimit(): UInt? = wallet?.renewLimit()

    /** Her consent to exit contracts ending, if given: none before a first contract opens. */
    suspend fun exitConsent(): Result<ExitConsent?> = io { local { it.exitConsent() } }

    /**
     * Consent to exit contracts ending, paying their notes out to
     * `address`, with the exit margin set now.
     */
    suspend fun consentToExit(address: String): Result<ExitConsent> = io {
        val margin = exitMarginBlocks(prefs.exitMarginDays)
        val given = local { it.consentToExit(address, margin) }
        AppLog.info("consented to exit contracts ending, margin $margin blocks")
        poke()
        given
    }

    /**
     * Withdraw to cold storage, the wallet of `key` (a zpub, or an xpub of
     * `script` "wpkh" or "tr"), as the rules say, its coins looked for
     * from `birthday`, by default the chain's height now: looked for and
     * withdrawn to on the next round.
     */
    suspend fun setCold(
        key: String,
        script: String?,
        threshold: ULong,
        keep: ULong,
        maxFeePercent: Double,
        birthday: UInt?,
    ): Result<ColdStorage> = io {
        val cold = locked { it.setCold(key, script, threshold, keep, maxFeePercent, birthday) }
        AppLog.info("cold storage set: past $threshold sat, keeping $keep, at most $maxFeePercent% in fees")
        state.update { it.copy(cold = cold) }
        poke()
        cold
    }

    /** Stop withdrawing to cold storage: what it holds stays there. */
    suspend fun clearCold(): Result<Unit> = io {
        locked { it.clearCold() }
        AppLog.info("cold storage cleared")
        state.update { it.copy(cold = null) }
    }

    /** Revoke her consent to exit: no step more taken. */
    suspend fun revokeExitConsent(): Result<Unit> = io {
        local { it.revokeExitConsent() }
        AppLog.info("consent to exit contracts revoked")
    }

    /** The exit margin set to `days`, her consent to exit given again with it if given. */
    suspend fun setExitMargin(days: Int): Result<Unit> = io {
        prefs.exitMarginDays = days
        local { it.exitConsent() }?.let { local { w -> w.consentToExit(it.address, exitMarginBlocks(days)) } }
        Unit
    }

    /** The wallet `w` opened, as its owner set it: what its renewals may cost. */
    private fun configured(w: Wallet): Wallet {
        prefs.renewPpm?.let { w.setRenewLimit(it) }
        // Its coins on Bitcoin looked for while the phone rests, too.
        ScanJob.schedule(app)
        return w
    }

    fun setDeveloper(on: Boolean) {
        prefs.developer = on
        state.update { it.copy(developer = on) }
    }

    /** The network of the wallet to make, its light client started now. */
    suspend fun setNetwork(network: Network): Result<Unit> = io {
        mutex.withLock {
            // A wallet there but failing to open keeps its network, and so
            // the Tor default that follows it.
            check(!walletExists(walletDir)) { "the wallet's network is chosen once" }
            prefs.network = network
        }
        lightLock.withLock { startLight() }
        refresh()
    }

    /** The light client's peers and block-dn server, from now on. */
    suspend fun setChain(peers: String, cdn: String): Result<Unit> = io {
        lightLock.withLock {
            prefs.peers = peers
            prefs.cdn = cdn
            startLight()
        }
        poke()
    }

    /** Forget the wallet: for developers, on test networks. */
    suspend fun delete(): Result<Unit> = io {
        mutex.withLock {
            wallet?.destroy()
            wallet = null
            told = null
            File(walletDir).deleteRecursively()
            AppLog.info("wallet deleted")
        }
        // Nothing of it under way any more: a new wallet starts afresh.
        synchronized(this) {
            prefs.watched = null
            prefs.warned = emptySet()
            prefs.askedAt = null
            seen = null
            looking = false
            WatchJob.cancel(app)
            ScanJob.cancel(app)
            // The folder's backups stay hers; none of a wallet gone.
            stopBackups()
        }
        // Its network may differ from the one chosen next.
        lightLock.withLock { startLight() }
        state.update { Ui(loaded = true, developer = prefs.developer, network = prefs.network) }
        refresh()
    }

    /** A call reaching the network or changing what the wallet holds. */
    private suspend fun <T> locked(call: (Wallet) -> T): T = mutex.withLock { local(call) }

    /** A call reading what the wallet holds, alongside any other. */
    private fun <T> local(call: (Wallet) -> T): T =
        call(wallet ?: throw LicashException.Invalid("there is no wallet yet"))

    private suspend fun <T> io(call: suspend () -> T): Result<T> =
        withContext(Dispatchers.IO) { runCatching { call() } }

    companion object {
        private const val ROUND_MS = 20_000L
        private const val OPENING_TICK_MS = 2_000L
        private const val EAGER_MS = 4_000L
        private const val MAIL_MS = 1_000L
        private const val AWAY_MS = 60_000L
        private const val CATCH_UP_AFTER_MS = 15_000L
        private const val BACKUP_REST_MS = 30_000L
        private const val BACKUP_MOST_MS = 5 * 60_000L
        private const val BACKUP_LOOK_MS = 3600_000L
        private const val BACKUP_RETRY_MS = 5 * 60_000L
        private const val PROVIDERS_MS = 15 * 60_000L
        private const val PROVIDERS_RETRY_MS = 2 * 60_000L
        private const val OFFER_MS = 30 * 60_000L
        private const val HUBS_MS = 5 * 60_000L
        private const val QUOTE_MS = 4 * 60_000L
    }
}

/** The coin `q` is offered for, by its outpoint: its largest. */
internal fun offeredCoin(q: ContractQuote): String? = q.coins.firstOrNull()?.outpoint

/** Whether the offer `q` was declined: its coin's, `declined` the coin last declined. */
internal fun offerDeclined(q: ContractQuote?, declined: String?): Boolean =
    q != null && declined != null && offeredCoin(q) == declined
