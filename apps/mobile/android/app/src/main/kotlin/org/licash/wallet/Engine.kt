package org.licash.wallet

import android.content.Context
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import java.io.File
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import org.licash.chain.mobile.Chain as LightClient
import org.licash.chain.mobile.Mobile
import org.licash.core.Activity
import org.licash.core.Balance
import org.licash.core.Code
import org.licash.core.Contract
import org.licash.core.ContractQuote
import org.licash.core.ContractState
import org.licash.core.Details
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
import org.licash.core.Paid
import org.licash.core.PayQuote
import org.licash.core.PayoutQuote
import org.licash.core.HubsFound
import org.licash.core.PayOffer
import org.licash.core.Scanned
import org.licash.core.State
import org.licash.core.Wallet
import org.licash.core.chainName
import org.licash.core.defaultRelays
import org.licash.core.explorer
import org.licash.core.setLogger
import org.licash.core.walletExists

/** What the screens show. */
data class Ui(
    val loaded: Boolean = false,
    val wallet: Boolean = false,
    val network: Network = Network.BITCOIN,
    val developer: Boolean = false,
    val balance: Balance = Balance(0u, 0u, 0u, 0u, 0u, 0u, 0u),
    val history: List<Activity> = emptyList(),
    val hubs: List<Hub> = emptyList(),
    val tor: Boolean = false,
    val chain: ChainStatus = ChainStatus(),
    /** The next block to look in for payments on Bitcoin, once looking. */
    val scanned: UInt? = null,
    /** Payments on Bitcoin on their way: in no block yet. */
    val incoming: List<Activity> = emptyList(),
    /** Why the last round of syncing fell short, if it did. */
    val problem: String? = null,
    /** The recovery contracts with hubs, the newest first. */
    val contracts: List<Contract> = emptyList(),
    /** The hub a contract is being opened at, while one is. */
    val opening: String? = null,
    /** Where each line of the history under way stands, by [key]. */
    val under: Map<String, Details> = emptyMap(),
)

/** A line of the history's own key: lines of different kinds can share an id. */
val Activity.key: String get() = "$kind $id"

/** At most so many lines under way have where they stand read, each refresh. */
private const val UNDER_READ = 20

data class ChainStatus(val height: Int = 0, val peers: Long = 0, val synced: Boolean = false)

/** Something to tell the user once; `received` if it is money in. */
data class Event(val text: String, val received: ULong? = null)

/**
 * The wallet and the light client, and the loop keeping them in step while
 * the app is in sight: looking for payments on Bitcoin, moving them into
 * Licash once there is enough, finishing what is under way and taking
 * what arrives in the mailbox. Every wallet call is on an IO thread; those
 * reaching the network or changing what the wallet holds one at a time,
 * those reading it alongside.
 */
class Engine(context: Context) {
    val prefs = Prefs(context)
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

    /** Look often: someone is waiting to be paid. */
    @Volatile
    var eager = false

    @Volatile
    private var foreground = false

    @Volatile
    private var wallet: Wallet? = null

    @Volatile
    private var light: LightClient? = null
    private var lightOf: String? = null

    /** The coins on Bitcoin and payments on their way already told of. */
    private var told: MutableSet<String>? = null

    /** How the light client stood when last logged. */
    private var logged: ChainStatus? = null

    /** Whether what the hubs trusted serve was learned since the app started. */
    @Volatile
    private var hubsLearned = false

    /** Carrying on with contracts, apart from the rounds: it may take minutes. */
    private val contracting = AtomicBoolean(false)

    init {
        AppLog.start(File(files, "logs"))
        setLogger(AppLog, "info,nostr_sdk=warn,nostr_relay_pool=warn")
        ProcessLifecycleOwner.get().lifecycle.addObserver(object : DefaultLifecycleObserver {
            override fun onStart(owner: LifecycleOwner) {
                foreground = true
                poke()
            }

            override fun onStop(owner: LifecycleOwner) {
                foreground = false
            }
        })
        scope.launch {
            load()
            loop()
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
                runCatching { configured(Wallet.open(walletDir, prefs.tor)) }
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
    }

    /** Run the light client of the wallet's network, or the one chosen. */
    private suspend fun startLight() {
        val network = wallet?.network() ?: prefs.network
        val key = listOf(chainName(network), prefs.peers, prefs.cdn).joinToString("|")
        if (key == lightOf && light != null) return
        // Between wallet calls: one may be reading through it.
        mutex.withLock { stopLight() }
        val name = chainName(network)
        val dir = File(files, "chain-$name").path
        AppLog.info("starting the light client of $name")
        runCatching { Mobile.start(name, dir, prefs.peers, prefs.cdn) }
            .onSuccess {
                light = it
                lightOf = key
                AppLog.info("light client of $name started")
            }
            .onFailure { e ->
                AppLog.warn("starting the light client", e)
                state.update { it.copy(problem = explain(e)) }
            }
    }

    private fun stopLight() {
        runCatching { light?.stop() }
        light = null
        lightOf = null
    }

    private suspend fun loop() {
        while (true) {
            if (foreground) {
                runCatching { sync() }.onFailure { e ->
                    AppLog.warn("sync", e)
                    state.update { it.copy(problem = explain(e)) }
                }
            }
            withTimeoutOrNull(if (eager) EAGER_MS else ROUND_MS) { wake.receive() }
        }
    }

    private suspend fun sync() {
        val light = light ?: return
        val status = ChainStatus(light.height(), light.peers(), light.synced())
        state.update { it.copy(chain = status) }
        var hubs = wallet?.let { local { w -> w.hubs() } } ?: return
        if (hubs.isEmpty()) return
        if (!hubsLearned) {
            learnHubs(hubs)
            hubs = local { it.hubs() }
        }
        val chain = GoChain(light)
        val problems = mutableListOf<String>()
        fun failed(step: String, e: LicashException) {
            AppLog.warn(step, e)
            problems += explain(e)
        }
        if (status != logged) {
            logged = status
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
                state.update { it.copy(scanned = scan.next) }
                if (scan.found > 0u) {
                    AppLog.info("found ${scan.found} coins of ${scan.foundValue} sat to block ${scan.next - 1u}")
                }
                if (scan.next > scan.height) {
                    caughtUp = true
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
        }
        if (hubs.any { it.mailbox }) {
            try {
                for (got in locked { it.collect() }) {
                    AppLog.info("received ${got.amount} sat in the mailbox")
                    notices.tryEmit(Event("Received ${sat(got.amount)}", got.amount))
                }
            } catch (e: LicashException) {
                failed("the mailbox", e)
            }
        }
        // Only with every coin found: a partial look could move part.
        if (caughtUp) {
            try {
                move(chain)
            } catch (e: LicashException) {
                failed("moving into Licash", e)
            }
        }
        state.update { it.copy(problem = problems.firstOrNull()) }
        refresh()
        tell()
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
    }

    /**
     * Move what was paid on Bitcoin into Licash, all at once, once it is
     * enough: but what was cashed out to this wallet, which stays.
     */
    private suspend fun move(chain: GoChain) {
        val balance = local { it.balance() }
        if (balance.onchain <= balance.kept || balance.moving > 0uL) return
        try {
            val moving = locked { it.moveToLicash(chain) }
            AppLog.info("moving ${moving.amount} sat into Licash")
            notices.tryEmit(Event("Moving ${sat(moving.amount)} into Licash"))
        } catch (_: LicashException.Insufficient) {
            // Too little for the hub yet: more payments may come.
        }
    }

    /**
     * Carry on with the contracts being opened and watch the funding of
     * those signed, in the background: an opening carried on downloads
     * for minutes. Nothing to do once each is confirmed or given up.
     */
    private fun resumeContracts(chain: GoChain) {
        val waiting = state.value.contracts.any {
            it.state == ContractState.Opening || it.state == ContractState.Confirming
        }
        if (!waiting || !contracting.compareAndSet(false, true)) return
        scope.launch {
            try {
                local { it.resumeContracts(chain) }
            } catch (e: Exception) {
                // Not to bring the app down from a coroutine of its own.
                AppLog.warn("carrying on with contracts", e)
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
                val history = it.history()
                // From the wallet alone, no network: Home tells each step.
                val under = history.filter { a -> a.state == State.PENDING }.take(UNDER_READ)
                    .mapNotNull { a -> runCatching { it.details(a.kind, a.id) }.getOrNull()?.let { d -> a.key to d } }
                    .toMap()
                Shown(it.network(), it.balance(), history, it.hubs(), it.tor(), contracts, under)
            }
        }.onFailure { AppLog.warn("reading the wallet", it) }.getOrNull()
        state.update {
            when {
                wallet == null -> it.copy(
                    wallet = false,
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
                    tor = shown.tor,
                    contracts = shown.contracts,
                    under = shown.under,
                )
            }
        }
    }

    private data class Shown(
        val network: Network,
        val balance: Balance,
        val history: List<Activity>,
        val hubs: List<Hub>,
        val tor: Boolean,
        val contracts: List<Contract>,
        val under: Map<String, Details>,
    )

    /** A new wallet of the network chosen, born at the chain's height if known. */
    suspend fun create(): Result<Unit> = io {
        mutex.withLock {
            val light = light
            val birthday = if (light != null && light.synced()) light.height().toUInt() else 0u
            wallet = configured(Wallet.create(walletDir, prefs.network, birthday, prefs.tor))
            AppLog.info("wallet made at block $birthday")
        }
        lightLock.withLock { startLight() }
        refresh()
        poke()
    }

    suspend fun addHub(url: String): Result<Hub> = io {
        val hub = locked { it.addHub(url) }
        refresh()
        poke()
        hub
    }

    suspend fun code(amount: ULong?): Result<Code> = io { local { it.paymentCode(amount) } }

    /** More of a line of the history, as far as the wallet looked. */
    suspend fun details(activity: Activity): Result<Details> =
        io { local { it.details(activity.kind, activity.id) } }

    /** The notes a line of the history spent, and those it made the wallet's. */
    suspend fun notesOf(activity: Activity): Result<NotesOf> =
        io { local { it.notesOf(activity.kind, activity.id) } }

    /** Where to see the transaction `txid`, if anywhere. */
    fun explorerLink(txid: String): String? {
        val base = when (val chosen = prefs.explorer) {
            "default" -> explorer(state.value.network)
            "" -> null
            else -> chosen
        }
        return base?.trimEnd('/')?.let { "$it/tx/$txid" }
    }

    suspend fun scanned(text: String): Result<Scanned> = io { local { it.scanned(text) } }

    /**
     * How paying `amount`, or what the code `code` asks, in notes goes, and
     * all it costs: [pay] pays it so. At a hub not trusted, the payee's,
     * visited and proving it is the one named, if no hub trusted is.
     */
    suspend fun payQuote(code: String, amount: ULong?): Result<PayQuote> =
        io { local { it.payQuote(code, amount) } }

    /** Every way of paying `code`: through each swap provider that could. */
    suspend fun payOffers(code: String, amount: ULong?): Result<List<PayOffer>> =
        io { local { it.payOffers(code, amount) } }

    /** Quote paying `code` through the provider at `provider`: paid so next. */
    suspend fun choosePayOffer(code: String, amount: ULong?, provider: String): Result<PayQuote> =
        io { local { it.choosePayOffer(code, amount, provider) } }

    suspend fun pay(code: String, amount: ULong?): Result<Paid> = io {
        val paid = locked { it.pay(code, amount) }
        refresh()
        paid
    }

    /** What paying the Lightning invoice `text` costs, through the hub paying it for least. */
    suspend fun invoiceQuote(text: String): Result<InvoiceQuote> = io { local { it.invoiceQuote(text) } }

    /** Pay the Lightning invoice `text` through `hub`: done, or under way. */
    suspend fun payInvoice(text: String, hub: String): Result<Activity> = io {
        val paid = locked { it.payInvoice(text, hub) }
        refresh()
        poke()
        paid
    }

    /** Ask the hub taking it for least for a Lightning invoice paying `amount` in notes. */
    suspend fun askInvoice(amount: ULong): Result<LightningInvoice> = io {
        val asked = locked { it.askInvoice(amount) }
        refresh()
        asked
    }

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

    /** Move all on Bitcoin into Licash now, what was cashed out here too. */
    suspend fun moveAll(): Result<Activity> = io {
        val light = light ?: throw LicashException.Unavailable("not connected to Bitcoin yet")
        val moving = locked { it.moveAllToLicash(GoChain(light)) }
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
    suspend fun contractQuote(hub: String): Result<ContractQuote> = io { local { it.contractQuote(hub) } }

    /**
     * Open a recovery contract at `hub` for at most `maxPrice`, in the
     * background: it takes minutes, its downloads large. Told once signed,
     * or why not. One at a time.
     */
    fun openContract(hub: String, maxPrice: ULong) {
        val light = light
        if (light == null) {
            notices.tryEmit(Event("Not connected to Bitcoin yet: try again in a moment."))
            return
        }
        var started = false
        state.update {
            started = it.opening == null
            if (started) it.copy(opening = hub) else it
        }
        if (!started) return
        AppLog.info("opening a contract at $hub")
        scope.launch {
            try {
                val opened = local { it.openContract(GoChain(light), hub, null, maxPrice) }
                AppLog.info("contract ${opened.id} signed at $hub, funded by ${opened.funding}")
                notices.tryEmit(Event("Contract signed at ${host(hub)}: protecting once its funding confirms"))
            } catch (e: Exception) {
                AppLog.warn("opening a contract at $hub", e)
                notices.tryEmit(Event("No contract opened. ${explain(e)}"))
            } finally {
                state.update { it.copy(opening = null) }
                refresh()
                poke()
            }
        }
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

    /** Reach hubs and relays through Tor, or not: the wallet opens again. */
    suspend fun setTor(on: Boolean): Result<Unit> = io {
        mutex.withLock {
            prefs.tor = on
            wallet?.let {
                it.destroy()
                wallet = configured(Wallet.open(walletDir, on))
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

    /** The wallet `w` opened, as its owner set it: what its renewals may cost. */
    private fun configured(w: Wallet): Wallet {
        prefs.renewPpm?.let { w.setRenewLimit(it) }
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
        private const val EAGER_MS = 4_000L
    }
}
