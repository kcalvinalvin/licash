package org.licash.wallet

import android.content.Context
import android.util.Log
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import java.io.File
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
import org.licash.core.Hub
import org.licash.core.LicashException
import org.licash.core.Network
import org.licash.core.Paid
import org.licash.core.Scanned
import org.licash.core.Wallet
import org.licash.core.chainName
import org.licash.core.setLogger
import org.licash.core.walletExists

/** What the screens show. */
data class Ui(
    val loaded: Boolean = false,
    val wallet: Boolean = false,
    val network: Network = Network.BITCOIN,
    val developer: Boolean = false,
    val balance: Balance = Balance(0u, 0u, 0u, 0u),
    val history: List<Activity> = emptyList(),
    val hubs: List<Hub> = emptyList(),
    val tor: Boolean = false,
    val chain: ChainStatus = ChainStatus(),
    /** The next block to look in for payments on Bitcoin, once looking. */
    val scanned: UInt? = null,
    /** Why the last round of syncing fell short, if it did. */
    val problem: String? = null,
)

data class ChainStatus(val height: Int = 0, val peers: Long = 0, val synced: Boolean = false)

/** Something to tell the user once; `received` if it is money in. */
data class Event(val text: String, val received: ULong? = null)

/**
 * The wallet and the light client, and the loop keeping them in step while
 * the app is in sight: looking for payments on Bitcoin, moving them into
 * Licash once there is enough, finishing what is under way and taking
 * what arrives in the mailbox. Every wallet call is on an IO thread, one
 * at a time.
 */
class Engine(context: Context) {
    val prefs = Prefs(context)
    private val files = context.filesDir
    private val walletDir = File(files, "wallet").path
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val mutex = Mutex()
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
    private var wallet: Wallet? = null
    private var light: LightClient? = null
    private var lightOf: String? = null

    init {
        setLogger(Lines, "info")
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

    private suspend fun load() {
        mutex.withLock {
            if (walletExists(walletDir)) {
                runCatching { Wallet.open(walletDir, prefs.tor) }
                    .onSuccess { wallet = it }
                    .onFailure { e -> state.update { it.copy(problem = explain(e)) } }
            }
            startLight()
        }
        refresh()
        state.update { it.copy(loaded = true) }
    }

    /** Run the light client of the wallet's network, or the one chosen. */
    private fun startLight() {
        val network = wallet?.network() ?: prefs.network
        val key = listOf(chainName(network), prefs.peers, prefs.cdn).joinToString("|")
        if (key == lightOf && light != null) return
        stopLight()
        val name = chainName(network)
        val dir = File(files, "chain-$name").path
        runCatching { Mobile.start(name, dir, prefs.peers, prefs.cdn) }
            .onSuccess {
                light = it
                lightOf = key
            }
            .onFailure { e -> state.update { it.copy(problem = explain(e)) } }
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
                    Log.w("licash", "sync", e)
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
        val hubs = mutex.withLock { wallet?.hubs() } ?: return
        if (hubs.isEmpty()) return
        val chain = GoChain(light)
        val problems = mutableListOf<String>()
        if (status.synced && hubs.any { it.silent }) {
            while (true) {
                val scan = try {
                    locked { it.scan(chain) }
                } catch (e: LicashException) {
                    problems += explain(e)
                    break
                }
                state.update { it.copy(scanned = scan.next) }
                if (scan.found > 0u) {
                    notices.tryEmit(Event("${sat(scan.foundValue)} arrived on Bitcoin", scan.foundValue))
                }
                if (scan.next > scan.height) break
            }
        }
        try {
            locked { it.resume() }
        } catch (e: LicashException) {
            problems += explain(e)
        }
        if (hubs.any { it.mailbox }) {
            try {
                for (got in locked { it.collect() }) {
                    notices.tryEmit(Event("Received ${sat(got.amount)}", got.amount))
                }
            } catch (e: LicashException) {
                problems += explain(e)
            }
        }
        try {
            move(chain)
        } catch (e: LicashException) {
            problems += explain(e)
        }
        state.update { it.copy(problem = problems.firstOrNull()) }
        refresh()
    }

    /** Move what was paid on Bitcoin into Licash, all at once, once it is enough. */
    private suspend fun move(chain: GoChain) {
        val balance = locked { it.balance() }
        if (balance.onchain == 0uL || balance.moving > 0uL) return
        try {
            val moving = locked { it.moveToLicash(chain) }
            notices.tryEmit(Event("Moving ${sat(moving.amount)} into Licash"))
        } catch (_: LicashException.Insufficient) {
            // Too little for the hub yet: more payments may come.
        }
    }

    private suspend fun refresh() {
        val shown = mutex.withLock {
            wallet?.let { Shown(it.network(), it.balance(), it.history(), it.hubs(), it.tor()) }
        }
        state.update {
            when (shown) {
                null -> it.copy(wallet = false, network = prefs.network, developer = prefs.developer)
                else -> it.copy(
                    wallet = true,
                    network = shown.network,
                    developer = prefs.developer,
                    balance = shown.balance,
                    history = shown.history,
                    hubs = shown.hubs,
                    tor = shown.tor,
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
    )

    /** A new wallet of the network chosen, born at the chain's height if known. */
    suspend fun create(): Result<Unit> = io {
        mutex.withLock {
            val light = light
            val birthday = if (light != null && light.synced()) light.height().toUInt() else 0u
            wallet = Wallet.create(walletDir, prefs.network, birthday, prefs.tor)
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

    suspend fun code(amount: ULong?): Result<Code> = io { locked { it.paymentCode(amount) } }

    suspend fun scanned(text: String): Result<Scanned> = io { locked { it.scanned(text) } }

    suspend fun pay(code: String, amount: ULong?): Result<Paid> = io {
        val paid = locked { it.pay(code, amount) }
        refresh()
        paid
    }

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
                wallet = Wallet.open(walletDir, on)
            }
        }
        refresh()
    }

    fun setDeveloper(on: Boolean) {
        prefs.developer = on
        state.update { it.copy(developer = on) }
    }

    /** The network of the wallet to make, its light client started now. */
    suspend fun setNetwork(network: Network): Result<Unit> = io {
        mutex.withLock {
            check(wallet == null) { "the wallet's network is chosen once" }
            prefs.network = network
            startLight()
        }
        refresh()
    }

    /** The light client's peers and block-dn server, from now on. */
    suspend fun setChain(peers: String, cdn: String): Result<Unit> = io {
        mutex.withLock {
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
            File(walletDir).deleteRecursively()
        }
        state.update { Ui(loaded = true, developer = prefs.developer, network = prefs.network) }
        refresh()
    }

    private suspend fun <T> locked(call: (Wallet) -> T): T = mutex.withLock {
        call(wallet ?: throw LicashException.Invalid("there is no wallet yet"))
    }

    private suspend fun <T> io(call: suspend () -> T): Result<T> =
        withContext(Dispatchers.IO) { runCatching { call() } }

    companion object {
        private const val ROUND_MS = 20_000L
        private const val EAGER_MS = 4_000L
    }
}
