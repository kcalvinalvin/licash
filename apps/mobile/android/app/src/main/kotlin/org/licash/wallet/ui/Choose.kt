package org.licash.wallet.ui

import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.dp
import org.licash.core.ContractQuote
import org.licash.core.ContractRange
import org.licash.core.DepositCoin
import org.licash.core.contractFee
import org.licash.core.contractFit
import org.licash.core.contractPrice
import org.licash.wallet.Chosen
import org.licash.wallet.Engine
import org.licash.wallet.digits
import org.licash.wallet.percent
import org.licash.wallet.sat
import kotlin.math.roundToLong

/**
 * The protection offered for what was paid on Bitcoin ([org.licash.wallet.Ui.offer]),
 * as [ChooseSheet]: opened once accepted, or declined so it is not offered
 * unasked again. None if no coin is taken whole by a contract within its
 * hub's range.
 */
@Composable
internal fun OfferSheet(engine: Engine, q: ContractQuote, dismiss: () -> Unit) {
    val range = q.range ?: return
    val fitting = q.coins.filter { fitOf(q, range, q.suggestedBlocks, it) != null }
    if (fitting.isEmpty()) return
    ChooseSheet(
        q,
        range,
        fitting,
        open = { chosen, price ->
            dismiss()
            engine.openContract(q.hub, price, chosen)
        },
        dismiss = dismiss,
        decline = {
            engine.declineOffer(q)
            dismiss()
        },
    )
}

/**
 * The contract within `range` over `blocks` that `coin` pays for whole
 * ([contractFit]): at least as large as the cover suggested, else the
 * least the hub allows; none if no contract takes the coin. The coin is spent
 * whole, the price and the rest of it her part, issued back to her as notes.
 */
internal fun fitOf(q: ContractQuote, range: ContractRange, blocks: UInt, coin: DepositCoin) =
    contractFit(range, blocks, coin.value, q.suggestedCap, q.dust, q.denominations, q.fundedNotes)
        ?: contractFit(range, blocks, coin.value, range.minCap, q.dust, q.denominations, q.fundedNotes)

/**
 * Protection of terms chosen within what its hub allows: for how long, at
 * first as `q` suggests, and paid from which of `coins`, the smallest that a
 * contract takes whole at first. A coin is spent whole: it pays the price,
 * and all the rest of it is her part of the contract, which the hub charges
 * its interest on none of and which joins her balance once its funding
 * confirms. What it covers is the smallest the hub allows that takes the
 * coin, shown beside what she funds and what it costs, itemized, as the hub
 * prices them. Offered unasked, it can be declined.
 *
 * With no coin to pay it, she is told to cash out to herself first.
 */
@Composable
internal fun ChooseSheet(
    q: ContractQuote,
    range: ContractRange,
    coins: List<DepositCoin>,
    open: (Chosen, price: ULong) -> Unit,
    dismiss: () -> Unit,
    decline: (() -> Unit)? = null,
) {
    val colors = MaterialTheme.colorScheme
    val download = android.text.format.Formatter.formatShortFileSize(LocalContext.current, q.download.toLong())
    var blocks by remember(q) { mutableStateOf(q.suggestedBlocks.coerceIn(range.minBlocks, range.maxBlocks)) }
    // At first the smallest coin that a contract takes whole; the smallest
    // of them all, to say why none does, if none.
    var coin by remember(q) {
        val sorted = coins.sortedBy { it.value }
        mutableStateOf(sorted.firstOrNull { fitOf(q, range, blocks, it) != null } ?: sorted.firstOrNull())
    }
    val fit = coin?.let { fitOf(q, range, blocks, it) }
    val capped = fit?.cap ?: range.minCap
    val funded = fit?.funded ?: 0uL
    val price = contractPrice(range, capped, blocks, funded)
    val fee = contractFee(range, capped, blocks, funded)
    // It stops protecting a little before it ends.
    val protectsFor = if (blocks > q.afterAssert) blocks - q.afterAssert else 0u
    Sheet("Protect at ${name(q.hub)}", dismiss) {
        Line("Covers up to", sat(capped), tag = "quote-cap")
        Line("For", span(protectsFor.toLong(), LocalBlockSeconds.current), tag = "quote-for")
        if (range.maxBlocks > range.minBlocks) {
            Slider(
                value = blocks.toFloat(),
                onValueChange = { blocks = blocksAt(it, range.minBlocks, range.maxBlocks) },
                valueRange = range.minBlocks.toFloat()..range.maxBlocks.toFloat(),
                modifier = Modifier.testTag("contract-blocks"),
            )
        }
        if (fit != null) {
            Line("You fund", sat(funded), tag = "contract-funded")
            Text(
                "Your coin is spent whole: what it pays beyond the price is yours, and joins your balance once confirmed. The hub charges interest on its own part only.",
                modifier = Modifier.padding(bottom = 8.dp),
                style = MaterialTheme.typography.bodySmall,
                color = colors.onSurfaceVariant,
            )
            Line("Hub's opening fee", sat(fee.opening), tag = "contract-opening-fee")
            Line(
                "Interest",
                "${sat(fee.interest)} · ${percent(range.ppmYear.toULong())} a year of ${digits(capped - funded)}",
                tag = "contract-interest",
            )
            if (funded > 0uL && (range.ppmYearFunded > 0u || fee.interestFunded > 0uL)) {
                Line(
                    "Interest on yours",
                    "${sat(fee.interestFunded)} · ${percent(range.ppmYearFunded.toULong())} a year",
                    tag = "contract-interest-funded",
                )
            }
            Line("Reserve, returned", sat(range.reserve), tag = "contract-reserve")
            FeeLine(
                "Cost", price, capped, tag = "quote-price", value = sat(price),
                breakdown = contractCost(range, capped, price, fee, funded),
            )
        }
        if (coins.size > 1) {
            Text("Paid from", modifier = Modifier.padding(top = 12.dp), style = MaterialTheme.typography.bodyLarge)
            Row(Modifier.horizontalScroll(rememberScrollState())) {
                for (c in coins) {
                    FilterChip(
                        selected = c == coin,
                        onClick = { coin = c },
                        label = { Text(sat(c.value)) },
                        modifier = Modifier.padding(end = 8.dp).testTag("contract-coin"),
                    )
                }
            }
        }
        if (coin == null) {
            Say("Paid from Bitcoin you hold, and you hold none yet. Receive some on Bitcoin, or cash out to yourself, then protect.")
        } else if (fit == null) {
            Text(
                NO_FIT,
                modifier = Modifier.padding(vertical = 8.dp).testTag("contract-problem"),
                style = MaterialTheme.typography.bodyMedium,
                color = colors.error,
            )
        }
        Line("Takes", "Up to an hour, $download")
        Text(
            DUTY,
            modifier = Modifier.padding(vertical = 8.dp).testTag("contract-duty"),
            style = MaterialTheme.typography.bodySmall,
            color = colors.onSurfaceVariant,
        )
        Worth(q.mock)
        Spacer(Modifier.height(8.dp))
        val c = coin
        if (c != null) {
            BigButton(
                "Protect for ${sat(price)}",
                Modifier.fillMaxWidth().testTag("protect"),
                enabled = fit != null,
            ) {
                open(Chosen(capped, blocks, c.outpoint), price)
            }
        }
        decline?.let {
            Spacer(Modifier.height(8.dp))
            BigButton("Not now", Modifier.fillMaxWidth().testTag("decline-offer"), quiet = true, onClick = it)
        }
    }
}

/** Why a coin pays for no contract: it is spent whole, so it is short of the price or beyond what the largest takes. */
internal const val NO_FIT = "No contract of this hub takes this coin whole: it is short of the price, " +
    "or beyond what the largest contract takes. Pay with another coin."

/** What protection asks of its owner, said before it is paid for. */
internal const val DUTY = "Open Licash now and then, and before it ends: what it protects " +
    "is recovered by this app, and is safe after it ends only once cashed out or protected again."

/** The blocks at `value` of a slider from `least` to `most`, in whole days but at its ends. */
internal fun blocksAt(value: Float, least: UInt, most: UInt): UInt {
    val days = (value / DAY_BLOCKS).roundToLong().coerceAtLeast(0L) * DAY_BLOCKS
    return days.toUInt().coerceIn(least, most)
}

/** Blocks in a day, about. */
private const val DAY_BLOCKS = 144L
