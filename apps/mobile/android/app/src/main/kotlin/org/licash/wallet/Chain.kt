package org.licash.wallet

import org.licash.chain.mobile.Chain as LightClient
import org.licash.core.ChainException
import org.licash.core.ChainSource

/** The Go light client as the wallet reads Bitcoin through it. */
class GoChain(private val light: LightClient) : ChainSource {
    override fun height(): UInt = call { light.height().toUInt() }

    override fun hash(height: UInt): ByteArray = call { light.hash(height.toInt()) }

    override fun time(height: UInt): Long = call { light.time(height.toInt()) }

    override fun filter(hash: ByteArray): ByteArray = call { light.filter(hash) }

    override fun block(hash: ByteArray): ByteArray = call { light.block(hash) }

    override fun broadcast(tx: ByteArray) = call { light.broadcast(tx) }

    override fun unconfirmed(script: ByteArray): ByteArray =
        call { light.unconfirmed(script) ?: ByteArray(0) }

    private inline fun <T> call(f: () -> T): T =
        try {
            f()
        } catch (e: Exception) {
            throw ChainException.Failed(e.message ?: "the light client failed")
        }
}
