package org.licash.wallet

import java.text.DateFormat
import java.util.Date
import java.util.Locale
import org.licash.core.Balance
import org.licash.core.ChainException
import org.licash.core.LicashException

/** `12,345 sat`, commas between thousands. */
fun sat(amount: ULong): String = "${digits(amount)} sat"

/** `12,345`. */
fun digits(amount: ULong): String = String.format(Locale.US, "%,d", amount.toLong())

/** All the wallet holds: in notes, and on Bitcoin. */
val Balance.total: ULong get() = notes + notesPending + onchain + moving + opening

/**
 * Of [total], what hubs owe on trust alone: the notes no contract covers.
 * The rest, on Bitcoin or covered by a contract, is the wallet's own.
 */
val Balance.credit: ULong
    get() {
        val held = notes + notesPending
        return if (held > covered) held - covered else 0uL
    }

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
    is LicashException.Invalid -> plain(e.reason)
    is LicashException.Unavailable -> plain(e.reason)
    is LicashException.Pending -> "Still under way: it finishes by itself."
    is LicashException.Insufficient ->
        "Not enough: ${sat(e.needed)} needed, ${sat(e.available)} available."
    is LicashException.NeedsTor -> "This hub is on Tor: turn Tor on in Settings."
    is LicashException.UnknownHub -> "This payment is at a hub you don't use yet: ${e.url}"
    is LicashException.Failed -> plain(e.reason)
    is ChainException.Failed -> plain(e.reason)
    else -> plain(e.message ?: e.toString())
}

/**
 * A reason the core gives, in the app's words: a sentence, its amounts
 * grouped, and no one to pay out or move said without naming providers.
 */
internal fun plain(reason: String): String {
    fun amount(m: MatchResult) = m.groupValues[1].toULongOrNull()?.let(::sat) ?: "${m.groupValues[1]} sat"
    NO_PAYOUT.matchEntire(reason)?.let { return "No one can send ${amount(it)} on Bitcoin right now. Try less, or later." }
    NO_MOVE.matchEntire(reason)?.let { return "No one can move ${amount(it)} between your hubs right now. Try less, or later." }
    return AMOUNT.replace(reason, ::amount).replaceFirstChar { it.uppercase() }
}

private val NO_PAYOUT = Regex("no swap provider found pays (\\d+) sat out on Bitcoin now")
private val NO_MOVE = Regex("no swap provider found moves (\\d+) sat from \\S+ to \\S+ now")
private val AMOUNT = Regex("\\b(\\d{4,}) sat\\b")

/**
 * The host of a hub's `url`, to name it short: with its port if it names
 * one, as hubs sharing a host do.
 */
fun host(url: String): String = runCatching {
    val uri = java.net.URI(url)
    val host = uri.host ?: return@runCatching null
    if (uri.port == -1) host else "$host:${uri.port}"
}.getOrNull() ?: url

/** About how long `blocks` take, ten minutes each: `about 6 days`. */
fun duration(blocks: Long): String {
    val minutes = blocks * 10
    return when {
        blocks <= 0 -> "now"
        minutes < 90 -> "about $minutes minutes"
        minutes < 48 * 60 -> "about ${(minutes + 30) / 60} hours"
        else -> "about ${(minutes + 720) / 1440} days"
    }
}

/** When, from seconds since 1970. */
fun at(seconds: Long): String =
    DateFormat.getDateTimeInstance(DateFormat.MEDIUM, DateFormat.SHORT).format(Date(seconds * 1000))
