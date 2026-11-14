package org.licash.wallet

import java.text.DateFormat
import java.util.Date
import java.util.Locale
import org.licash.core.ChainException
import org.licash.core.LicashException

/** `12 345 sat`, thin spaces between thousands. */
fun sat(amount: ULong): String = "${digits(amount)} sat"

fun digits(amount: ULong): String =
    String.format(Locale.US, "%,d", amount.toLong()).replace(',', '\u2009')

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

/** The host of a hub's `url`, to name it short. */
fun host(url: String): String = runCatching { java.net.URI(url).host }.getOrNull() ?: url

/** When, from seconds since 1970. */
fun at(seconds: Long): String =
    DateFormat.getDateTimeInstance(DateFormat.MEDIUM, DateFormat.SHORT).format(Date(seconds * 1000))
