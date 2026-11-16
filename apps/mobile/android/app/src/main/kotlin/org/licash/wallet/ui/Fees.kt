package org.licash.wallet.ui

import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import org.licash.core.Rate
import org.licash.wallet.percent
import org.licash.wallet.sat
import org.licash.wallet.share
import java.math.BigInteger

/** Only supplied or recorded components, never inferred from current terms. */
internal data class FeeBreakdown(
    val amount: ULong,
    val fee: ULong,
    val components: List<Pair<String, ULong>> = emptyList(),
    val rate: Rate? = null,
    val explanation: String = "This total includes the quoted costs. Separate hub, provider and miner charges were not supplied.",
    val title: String = "Fee breakdown",
    val totalLabel: String = "Total fee",
    val amountLabel: String = "Amount",
)

internal fun recordedFee(amount: ULong, fee: ULong, components: List<Pair<String, ULong>> = emptyList()) = FeeBreakdown(
    amount, fee, components,
    explanation = "Recorded cost only. Base/rate terms and any further recipient split were not saved. A pending or refunded cost may not be final.",
)

internal fun setupCost(amount: ULong, cost: ULong) = FeeBreakdown(
    amount, cost,
    explanation = "Setup cost relative to the maximum payout, not a payment fee. A separate hub-fee and reserve split is not supplied here; reserves may return during recovery.",
    title = "Setup cost", totalLabel = "Total cost", amountLabel = "Maximum payout",
)

/**
 * A fee's parts by who gets them, as quoted or recorded: those of 0 left
 * out, and none at all unless they add up to the whole fee.
 */
internal fun feeParts(fee: ULong, parts: List<Pair<String, ULong>>): List<Pair<String, ULong>> {
    val shown = parts.filter { it.second > 0uL }
    val sum = shown.fold(0uL) { total, (_, part) -> if (ULong.MAX_VALUE - total < part) ULong.MAX_VALUE else total + part }
    return if (shown.isNotEmpty() && sum == fee) shown else emptyList()
}

/**
 * A payout's fee: the hub's on the lock, the payout provider's (its coin's
 * miners in it) and the claim's miners; `quoted`, before it is made.
 */
internal fun payoutFee(amount: ULong, fee: ULong, hub: ULong, provider: ULong, network: ULong, quoted: Boolean): FeeBreakdown {
    val parts = feeParts(fee, listOf("Hub" to hub, "Payout provider" to provider, "Bitcoin network" to network))
    val provided = "The payout provider's fee includes the miners of its own coin."
    return when {
        parts.isEmpty() -> if (quoted) FeeBreakdown(amount, fee) else recordedFee(amount, fee)
        quoted -> FeeBreakdown(amount, fee, parts, explanation = provided)
        else -> FeeBreakdown(amount, fee, parts, explanation = "As recorded. $provided Base and rate terms were not saved.")
    }
}

/** A move's fee as recorded: the hub's on the lock and the swap provider's. */
internal fun swapFee(amount: ULong, fee: ULong, hub: ULong, provider: ULong): FeeBreakdown {
    val parts = feeParts(fee, listOf("Hub" to hub, "Swap provider" to provider))
    return if (parts.isEmpty()) recordedFee(amount, fee)
    else FeeBreakdown(amount, fee, parts, explanation = "As recorded. Base and rate terms were not saved.")
}

/** Match the SDK's base plus relative fee, rounded up and saturating. */
internal fun relativeFee(amount: ULong, ppm: ULong): ULong {
    val numerator = amount.toString().toBigInteger() * ppm.toString().toBigInteger()
    return ((numerator + BigInteger.valueOf(999_999)) / BigInteger.valueOf(1_000_000))
        .min(ULong.MAX_VALUE.toString().toBigInteger()).toString().toULong()
}

internal fun feeDue(amount: ULong, rate: Rate): ULong {
    val relative = relativeFee(amount, rate.ppm)
    return if (ULong.MAX_VALUE - relative < rate.base) ULong.MAX_VALUE else relative + rate.base
}

@Composable
internal fun FeeLine(
    label: String,
    fee: ULong,
    amount: ULong,
    tag: String = "fee",
    value: String = if (fee == 0uL) "None" else sat(fee),
    breakdown: FeeBreakdown = FeeBreakdown(amount, fee),
) {
    var showing by remember { mutableStateOf(false) }
    Line(label, value, tag = tag, share = share(fee, amount), onClick = { showing = true })
    if (showing) FeeSheet(breakdown) { showing = false }
}

@Composable
internal fun FeeSheet(fee: FeeBreakdown, dismiss: () -> Unit) {
    Sheet(fee.title, dismiss) {
        Line(fee.amountLabel, sat(fee.amount))
        fee.components.forEach { (recipient, amount) ->
            Line(recipient, sat(amount), share = share(amount, fee.amount))
        }
        fee.rate?.let { rate ->
            Line("Base fee", sat(rate.base))
            Line("Rate", percent(rate.ppm))
            val relative = relativeFee(fee.amount, rate.ppm)
            Line("${percent(rate.ppm)} × ${sat(fee.amount)}", sat(relative))
            Say("The relative fee is rounded up to a whole satoshi.")
        }
        Line(fee.totalLabel, sat(fee.fee), share = share(fee.fee, fee.amount))
        Say(fee.explanation)
    }
}
