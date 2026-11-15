package org.licash.wallet.ui

import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import java.time.format.FormatStyle
import java.util.Locale
import org.licash.core.Balance
import org.licash.core.Contract
import org.licash.core.ContractState
import org.licash.core.Rate
import org.licash.wallet.credit
import org.licash.wallet.host
import org.licash.wallet.percent
import org.licash.wallet.sat
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
    zone: ZoneId = ZoneId.systemDefault(),
    locale: Locale = Locale.getDefault(),
): String {
    val at = Instant.ofEpochSecond(seconds).atZone(zone)
    val date = at.toLocalDate()
    val today = Instant.ofEpochSecond(now).atZone(zone).toLocalDate()
    val format = { pattern: String -> at.format(DateTimeFormatter.ofPattern(pattern, locale)) }
    return when {
        date == today -> at.format(
            DateTimeFormatter.ofLocalizedTime(FormatStyle.SHORT).withLocale(locale),
        )
        date == today.minusDays(1) -> "Yesterday"
        date.year == today.year -> format("MMM d")
        else -> format("MMM d, yyyy")
    }
}

/**
 * A hub's name, short, from its `url`: its host, with its port if it names
 * one; an onion service's by its first letters, the rest telling nobody
 * anything.
 */
fun name(url: String): String {
    val host = host(url)
    val onion = host.substringBefore(':').removeSuffix(".onion")
    return if (onion.length > 12 && host.substringBefore(':').endsWith(".onion")) {
        onion.take(8) + "….onion" + host.substring(host.substringBefore(':').length)
    } else {
        host
    }
}

/** A fee, as short as it can be said: `None`, `10 sat`, `0.5%`, `10 sat + 0.5%`. */
fun charge(rate: Rate): String = when {
    rate.base == 0uL && rate.ppm == 0uL -> "None"
    rate.ppm == 0uL -> sat(rate.base)
    rate.base == 0uL -> percent(rate.ppm)
    else -> "${sat(rate.base)} + ${percent(rate.ppm)}"
}

/** What protection under the mock authority is worth, said wherever one shows. */
const val MOCK = "Test only: protects nothing"

/** What genuine protection can do for now, said wherever one shows. */
const val UNRECOVERED = "Recovery is not in this app yet: treat as not protected"

/**
 * A protection as a row says it: where it `stands`, in a word; whether it
 * is `on`, protecting now, `runs`, protecting or about to, or `failed`;
 * whether the hub's bitcoin is `held` behind it; how long it has `left` to
 * protect, and in how long it `ends`, the hub's bitcoin back to it, while
 * the chain's height is known.
 */
data class Cover(
    val stands: String,
    val on: Boolean,
    val runs: Boolean,
    val failed: Boolean,
    val held: Boolean,
    val left: String?,
    val ends: String?,
)

/** `contract`, in words, at the chain's `height`: 0 while unknown. */
fun cover(contract: Contract, height: Int): Cover {
    val state = contract.state
    val on = state == ContractState.Protecting
    val runs = on || state == ContractState.Opening || state == ContractState.Confirming
    val held = on || state == ContractState.Confirming || state == ContractState.Ending
    val known = height > 0
    return Cover(
        stands = when (state) {
            ContractState.Opening -> "Setting up"
            ContractState.Confirming -> "Confirming"
            // A genuine one protects nothing this app can claim yet: live only.
            ContractState.Protecting -> if (contract.mock) "Protecting" else "Live"
            ContractState.Ending -> "Ending"
            ContractState.Closed -> "Ended"
            is ContractState.Failed -> "Failed"
        },
        on = on,
        runs = runs,
        failed = state is ContractState.Failed,
        held = held,
        left = if (runs && known) "${span(contract.protectsUntil.toLong() - height)} left" else null,
        ends = if ((runs || held) && known) span(contract.closesAt.toLong() - height) else null,
    )
}

/**
 * Whether `contract` shows outside advanced mode at `now`, in seconds
 * since 1970: not once ended, nor failed over a day ago.
 */
fun shown(contract: Contract, now: Long): Boolean = when (contract.state) {
    ContractState.Closed -> false
    is ContractState.Failed -> now - contract.createdAt < 86_400
    else -> true
}

/** What a payout to this wallet leaves as change, protecting from it. */
private const val FIRST_CHANGE: ULong = 10_000uL

/** The coin to cash out to this wallet to pay a protection of `price`: it, and some change. */
fun firstCoin(price: ULong): ULong = price + FIRST_CHANGE

/** `amount` sat in bitcoin, as a payment link says it: `0.00033000`. */
fun btc(amount: ULong): String =
    "%d.%08d".format(Locale.US, (amount / 100_000_000uL).toLong(), (amount % 100_000_000uL).toLong())
