package org.licash.wallet

import org.licash.core.ContractSafety

/**
 * A margin in days, as blocks: Bitcoin's 144 a day on every network, its
 * safety counted in blocks like the contracts' own. Not a wall-clock
 * promise: on Mutinynet, whose blocks come 20 times as quick, it is the
 * same blocks and so a twentieth of the time.
 */
internal fun exitMarginBlocks(days: Int): UInt {
    require(days in 1..30)
    return days.toUInt() * BLOCKS_PER_DAY
}

private const val BLOCKS_PER_DAY = 144u

/**
 * How often a forced close needs the wallet online: its signed challenge
 * window of `blocks`, of `blockSecs` each, in estimated hours.
 */
internal fun checkInHours(blocks: UInt, blockSecs: UInt): UInt =
    maxOf(1u, (blocks.toULong() * blockSecs.toULong() / 3600uL).toUInt())

/** The displayed deadline is for R to confirm, not for its broadcast to start. */
internal fun safetyStatus(safety: ContractSafety, height: Int): String = when {
    height <= 0 -> "Chain height unknown"
    safety.requestBy == null -> "Margin does not fit this contract"
    height.toUInt() > safety.requestBy!! -> "Inside your recovery margin"
    else -> "Request should confirm by block ${safety.requestBy}"
}
