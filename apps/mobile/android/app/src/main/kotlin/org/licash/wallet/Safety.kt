package org.licash.wallet

import org.licash.core.ContractSafety
import org.licash.core.Network

/** Estimated days converted to consensus blocks, not a wall-clock promise. */
internal fun proofMarginBlocks(days: Int, network: Network): UInt {
    require(days in 1..30)
    return days.toUInt() * if (network == Network.MUTINYNET) 2_880u else 144u
}

/** The displayed deadline is for R to confirm, not for its broadcast to start. */
internal fun safetyStatus(safety: ContractSafety, height: Int): String = when {
    height <= 0 -> "Chain height unknown"
    safety.requestBy == null -> "Margin does not fit this contract"
    height.toUInt() > safety.requestBy!! -> "Inside your recovery margin"
    else -> "Request should confirm by block ${safety.requestBy}"
}
