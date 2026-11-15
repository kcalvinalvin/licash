package org.licash.wallet

import java.text.DateFormat
import java.util.Date
import java.util.Locale
import org.licash.core.Balance
import org.licash.core.ChainException
import org.licash.core.LicashException

/** `12 345 sat`, thin spaces between thousands. */
fun sat(amount: ULong): String = "${digits(amount)} sat"

fun digits(amount: ULong): String =
    String.format(Locale.US, "%,d", amount.toLong()).replace(',', '\u2009')

/** All the wallet holds: in notes, and on Bitcoin. */
val Balance.total: ULong get() = notes + notesPending + onchain + moving

/**
 * Of [total], what hubs owe on trust alone: the notes, which no contract
 * covers yet. The rest, on Bitcoin, is the wallet's own.
 */
val Balance.credit: ULong get() = notes + notesPending

/**
 * [credit] as a share of [total], in whole percent: 0 if nothing is held.
 * Never 0 with some on credit, nor 100 with some protected.
 */
val Balance.creditPercent: Int
    get() {
        if (total == 0uL) return 0
        val rounded = ((credit.toDouble() * 100 / total.toDouble()) + 0.5).toInt()
        return when {
            credit > 0uL && rounded == 0 -> 1
            credit < total && rounded == 100 -> 99
            else -> rounded
        }
    }

/** `ppm` millionths as a percentage: `0.5%`. */
fun percent(ppm: ULong): String =
    java.text.DecimalFormat("0.####", java.text.DecimalFormatSymbols(Locale.US))
        .format(ppm.toDouble() / 10_000) + "%"

/** What went wrong, for the user. */
fun explain(e: Throwable): String = when (e) {
    is LicashException.Invalid -> sentence(e.reason)
    is LicashException.Unavailable -> sentence(e.reason)
    is LicashException.Pending -> "Still under way: it finishes by itself."
    is LicashException.Insufficient ->
        "Not enough: ${sat(e.needed)} needed, ${sat(e.available)} available."
    is LicashException.NeedsTor -> "This hub is on Tor: turn Tor on in Settings."
    is LicashException.UnknownHub -> "This payment is at a hub you don't use yet: ${e.url}"
    is LicashException.Failed -> sentence(e.reason)
    is ChainException.Failed -> sentence(e.reason)
    else -> sentence(e.message ?: e.toString())
}

private fun sentence(text: String): String = text.replaceFirstChar { it.uppercase() }

/**
 * The host of a hub's `url`, to name it short: with its port if it names
 * one, as hubs sharing a host do.
 */
fun host(url: String): String = runCatching {
    val uri = java.net.URI(url)
    val host = uri.host ?: return@runCatching null
    if (uri.port == -1) host else "$host:${uri.port}"
}.getOrNull() ?: url

/** When, from seconds since 1970. */
fun at(seconds: Long): String =
    DateFormat.getDateTimeInstance(DateFormat.MEDIUM, DateFormat.SHORT).format(Date(seconds * 1000))
