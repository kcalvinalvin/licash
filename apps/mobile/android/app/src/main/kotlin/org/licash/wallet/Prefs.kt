package org.licash.wallet

import android.content.Context
import org.licash.core.Network

/** What the app keeps besides the wallet: settings, mostly for developers. */
class Prefs(context: Context) {
    private val prefs = context.getSharedPreferences("licash", Context.MODE_PRIVATE)

    /** Shows the test networks and the light client's settings. */
    var developer: Boolean
        get() = prefs.getBoolean("developer", false)
        set(on) = prefs.edit().putBoolean("developer", on).apply()

    /** The network a new wallet is made for. */
    var network: Network
        get() = runCatching { Network.valueOf(prefs.getString("network", null)!!) }
            .getOrDefault(Network.BITCOIN)
        set(network) = prefs.edit().putString("network", network.name).apply()

    /** The only peers of the light client, comma separated, or none. */
    var peers: String
        get() = prefs.getString("peers", "")!!
        set(peers) = prefs.edit().putString("peers", peers.trim()).apply()

    /** The block-dn server to sync from: "default", a URL, or empty. */
    var cdn: String
        get() = prefs.getString("cdn", "default")!!
        set(cdn) = prefs.edit().putString("cdn", cdn.trim()).apply()

    /** The block explorer transactions link to: "default", a URL, or empty for none. */
    var explorer: String
        get() = prefs.getString("explorer", "default")!!
        set(url) = prefs.edit().putString("explorer", url.trim()).apply()

    /** Hubs and relays reached through Tor. */
    var tor: Boolean
        get() = prefs.getBoolean("tor", false)
        set(on) = prefs.edit().putBoolean("tor", on).apply()

    /**
     * The most paid to renew notes a hub that restarted no longer unlinks,
     * in parts per million of them: none if never set, the wallet's own.
     */
    var renewPpm: UInt?
        get() = prefs.getLong("renew_ppm", -1L).takeIf { it >= 0 }?.toUInt()
        set(ppm) = prefs.edit().putLong("renew_ppm", ppm?.toLong() ?: -1L).apply()
}
