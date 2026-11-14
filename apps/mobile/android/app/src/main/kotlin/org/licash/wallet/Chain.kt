package org.licash.wallet

import android.util.Log
import org.licash.chain.mobile.Chain as LightClient
import org.licash.core.ChainException
import org.licash.core.ChainSource
import org.licash.core.Logger

/** The Go light client as the wallet reads Bitcoin through it. */
class GoChain(private val light: LightClient) : ChainSource {
    override fun height(): UInt = call { light.height().toUInt() }

    override fun hash(height: UInt): ByteArray = call { light.hash(height.toInt()) }

    override fun filter(hash: ByteArray): ByteArray = call { light.filter(hash) }

    override fun block(hash: ByteArray): ByteArray = call { light.block(hash) }

    override fun broadcast(tx: ByteArray) = call { light.broadcast(tx) }

    private inline fun <T> call(f: () -> T): T =
        try {
            f()
        } catch (e: Exception) {
            throw ChainException.Failed(e.message ?: "the light client failed")
        }
}

/** The wallet's log: to logcat, and the newest lines for the developer screen. */
object Lines : Logger {
    private const val KEEP = 300
    private val recent = ArrayDeque<String>()

    override fun log(level: String, line: String) {
        val priority = when (level) {
            "ERROR" -> Log.ERROR
            "WARN" -> Log.WARN
            "DEBUG" -> Log.DEBUG
            "TRACE" -> Log.VERBOSE
            else -> Log.INFO
        }
        Log.println(priority, "licash", line)
        synchronized(recent) {
            recent.addLast(line)
            while (recent.size > KEEP) recent.removeFirst()
        }
    }

    fun recent(): List<String> = synchronized(recent) { recent.toList() }
}
