package org.licash.wallet.ui

import org.licash.core.Balance
import org.licash.wallet.credit
import org.licash.wallet.total

/*
 * The app's words, where a screen does not own them: what is said the same
 * way wherever it shows. Protected, of what the wallet holds, is what is
 * its own whatever a hub does: on Bitcoin, or covered by a hub's
 * collateral. The rest is not protected: owed by a hub, on its word alone.
 * Pure, to be tested without a screen.
 */

/** How much of what the wallet holds is protected: all, some or none of it. */
enum class Shield { ALL, SOME, NONE }

/** What the wallet holds, as `protected` and `open`, the rest: in sat. */
data class Protection(val protected: ULong, val open: ULong) {
    val total: ULong get() = protected + open

    /** None while nothing is held: nothing to say of it. */
    val shield: Shield?
        get() = when {
            total == 0uL -> null
            open == 0uL -> Shield.ALL
            protected == 0uL -> Shield.NONE
            else -> Shield.SOME
        }

    /**
     * [protected] as a share of [total], in whole percent: never 0 with
     * some protected, nor 100 with some not.
     */
    val percent: Int
        get() {
            if (total == 0uL) return 0
            val rounded = ((protected.toDouble() * 100 / total.toDouble()) + 0.5).toInt()
            return when {
                protected > 0uL && rounded == 0 -> 1
                open > 0uL && rounded == 100 -> 99
                else -> rounded
            }
        }

    /** In a word or two, as Home says it. */
    val label: String?
        get() = when (shield) {
            null -> null
            Shield.ALL -> PROTECTED
            Shield.NONE -> NOT_PROTECTED
            Shield.SOME -> "$percent% protected"
        }
}

/** How much of `balance` is protected. */
fun protection(balance: Balance): Protection = Protection(balance.total - balance.credit, balance.credit)

const val PROTECTED = "Protected"
const val NOT_PROTECTED = "Not protected"

/** What each means, where asked. */
const val PROTECTED_MEANS =
    "Yours whatever happens: on Bitcoin, or backed by bitcoin a hub has locked for you."
const val NOT_PROTECTED_MEANS =
    "Held on credit: a hub owes it to you, on its word alone. Protect it, or cash it out to Bitcoin."

/**
 * How long `blocks` take, ten minutes each, as a clock would say it: `6
 * days`. Never a block height: what a block is, nobody needs to know.
 */
fun span(blocks: Long): String {
    val minutes = blocks * 10
    val hours = (minutes + 30) / 60
    val days = (minutes + 720) / 1440
    return when {
        blocks <= 0 -> "now"
        minutes < 60 -> "$minutes min"
        hours < 48 -> if (hours == 1L) "1 hour" else "$hours hours"
        else -> "$days days"
    }
}

/**
 * When `seconds` since 1970 was, as short as `now` lets it be said: the
 * time today, `Yesterday`, `Nov 13`, and with its year once it is another.
 */
fun day(
    seconds: Long,
    now: Long,
    zone: java.time.ZoneId = java.time.ZoneId.systemDefault(),
    locale: java.util.Locale = java.util.Locale.getDefault(),
): String {
    val at = java.time.Instant.ofEpochSecond(seconds).atZone(zone)
    val date = at.toLocalDate()
    val today = java.time.Instant.ofEpochSecond(now).atZone(zone).toLocalDate()
    val format = { pattern: String -> at.format(java.time.format.DateTimeFormatter.ofPattern(pattern, locale)) }
    return when {
        date == today -> at.format(
            java.time.format.DateTimeFormatter.ofLocalizedTime(java.time.format.FormatStyle.SHORT).withLocale(locale),
        )
        date == today.minusDays(1) -> "Yesterday"
        date.year == today.year -> format("MMM d")
        else -> format("MMM d, yyyy")
    }
}
