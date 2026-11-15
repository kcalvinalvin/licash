package org.licash.wallet

import org.licash.core.Activity
import org.licash.core.ContractState
import org.licash.core.Details
import org.licash.core.Kind
import org.licash.core.State

/*
 * What keeps the app at work out of sight, and what it tells of then. A
 * payment under way finishes only while the app runs: a payout's coin is
 * claimed before its deadline, notes locked to the wallet are claimed, a
 * deposit is swapped in. Pure, apart from Android: the engine acts on it.
 */

/** How closely to watch what is under way. */
enum class Watch {
    /** Nothing under way. */
    NONE,

    /** Nothing urgent: a look now and then. */
    LATER,

    /** A payment the app finishes: kept at work, in a foreground service. */
    NOW,
}

/**
 * Lines under way for longer are stuck, their hub or provider gone: nothing
 * left to finish in time, and not to keep the app at work for ever.
 */
internal const val STALE_SECS = 7 * 24 * 3600L

/** A Lightning invoice asked is looked for so long after: perhaps paid out of sight. */
internal const val ASKED_SECS = 24 * 3600L

/** How closely to watch what `ui` shows under way, at `now` in seconds since the epoch. */
internal fun watchOf(ui: Ui, now: Long): Watch {
    val contracting = ui.opening != null || ui.balance.moving > 0uL ||
        ui.contracts.any { it.state == ContractState.Opening || it.state == ContractState.Confirming }
    // Of unknown age: perhaps not stuck.
    val under = ui.history.filter { it.state == State.PENDING && (it.at ?: now) > now - STALE_SECS }
    val asked = ui.asked?.let { it > now - ASKED_SECS } ?: false
    return when {
        contracting || under.any { !unpaid(it, ui.under[it.key]) } -> Watch.NOW
        under.isNotEmpty() || asked -> Watch.LATER
        else -> Watch.NONE
    }
}

/**
 * A deposit whose own payment on Bitcoin was never sent: it ends only once
 * its hub cancels it. Where it stands unread, perhaps sent.
 */
internal fun unpaid(activity: Activity, details: Details?): Boolean =
    activity.kind == Kind.MOVED && details is Details.Moved && details.payment == null

/** The lines finished since last looked, and those under way now, by [key]. */
internal data class Look(val finished: List<Activity>, val under: Set<String>)

/**
 * Compare `history` with the lines under way `before`, and with every line
 * `seen` last time if known. Never looked before (`before` null, as on the
 * first start of an app that did not keep them): none finished, so nothing
 * done long ago is told of as new. A Lightning invoice paid and its notes
 * taken at once, never seen under way, finished too if not `seen` before.
 */
internal fun look(history: List<Activity>, before: Set<String>?, seen: Set<String>? = null): Look {
    val under = history.filter { it.state == State.PENDING }.map { it.key }.toSet()
    val finished = before?.let { b ->
        history.filter {
            it.state != State.PENDING &&
                (it.key in b || (seen != null && it.kind == Kind.INVOICE && it.key !in seen))
        }
    }
    return Look(finished.orEmpty(), under)
}

/**
 * What to tell of a line finished, if anything, in a notification: amounts
 * only, never why one failed (it may name a hub or a transaction); a coin
 * moving in shows as its deposit.
 */
internal fun news(activity: Activity): String? {
    val amount = sat(activity.amount)
    return when (activity.state) {
        State.PENDING -> null
        State.DONE -> when (activity.kind) {
            Kind.SENT -> "Sent $amount"
            Kind.LIGHTNING -> "Paid $amount over Lightning"
            Kind.PAYOUT -> "Sent $amount on Bitcoin"
            Kind.SWAP -> "Moved $amount between hubs"
            Kind.MOVED -> "$amount moved into Licash"
            Kind.INVOICE -> "Received $amount over Lightning"
            Kind.CONTRACT -> "Contract opened: protecting"
            Kind.RECEIVED, Kind.ONCHAIN -> null
        }
        State.FAILED -> when (activity.kind) {
            Kind.SENT -> "Sending $amount"
            Kind.LIGHTNING -> "Paying $amount over Lightning"
            Kind.PAYOUT -> "Sending $amount on Bitcoin"
            Kind.SWAP -> "Moving $amount between hubs"
            Kind.MOVED -> "Moving $amount into Licash"
            Kind.INVOICE -> "Receiving $amount over Lightning"
            Kind.CONTRACT -> "Opening a contract"
            Kind.RECEIVED, Kind.ONCHAIN -> null
        }?.let { "$it failed: open Licash for why" }
    }
}

/** Blocks before a payout's deadline to warn at: about two hours. */
internal const val WARN_BLOCKS = 12u

/**
 * The payouts whose provider sent its coin, not claimed yet within
 * [WARN_BLOCKS] of the block to claim it by: that block, by [key]. Past it,
 * the provider may take its coin back and the payout fails.
 */
internal fun nearing(ui: Ui): Map<String, UInt> = buildMap {
    for (activity in ui.history) {
        if (activity.kind != Kind.PAYOUT || activity.state != State.PENDING) continue
        val steps = (ui.under[activity.key] as? Details.Payout)?.progress ?: continue
        val by = steps.claimBy ?: continue
        val tip = maxOf(steps.tip, ui.chain.height.coerceAtLeast(0).toUInt())
        if (steps.coin != null && steps.claim == null && tip + WARN_BLOCKS >= by) put(activity.key, by)
    }
}
