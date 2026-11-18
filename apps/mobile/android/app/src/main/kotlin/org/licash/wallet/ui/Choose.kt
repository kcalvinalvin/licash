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
import org.licash.core.contractFundable
import org.licash.core.contractLeastCoin
import org.licash.core.contractMaxCap
import org.licash.core.contractMostFunded
import org.licash.core.contractPrice
import org.licash.core.contractSizes
import org.licash.core.payableSize
import org.licash.wallet.Chosen
import org.licash.wallet.Engine
import org.licash.wallet.digits
import org.licash.wallet.percent
import org.licash.wallet.sat
import kotlin.math.roundToInt
import kotlin.math.roundToLong

/**
 * The protection offered for what was paid on Bitcoin ([org.licash.wallet.Ui.offer]),
 * as [ChooseSheet]: opened once accepted, or declined so it is not offered
 * unasked again. None if no coin pays one within its hub's range.
 */
@Composable
internal fun OfferSheet(engine: Engine, q: ContractQuote, dismiss: () -> Unit) {
    val range = q.range ?: return
    val paying = q.coins.filter { contractMaxCap(range, it.value) != null }
    if (paying.isEmpty()) return
    ChooseSheet(
        q,
        range,
        paying,
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
 * Protection of terms chosen within what its hub allows: which of its
 * sizes it covers and for how long, at first as `q` suggests (covering
 * less if the coin pays no more, [payableSize]), and how much of it she
 * funds herself, at first all her coin pays beside the price: the hub
 * charges its interest on the rest only, and her part joins her balance
 * once its funding confirms. What is left of the coin comes back on
 * Bitcoin. Priced as the hub prices them, itemized, and paid from which
 * of `coins`, if there are several. Offered unasked, it can be declined.
 *
 * With no coin to pay it, the terms are chosen all the same, at first
 * covering the `held` notes, and the coin they need
 * ([contractLeastCoin]) is cashed out first (`cashOutFirst`), if `held`
 * pays it.
 */
@Composable
internal fun ChooseSheet(
    q: ContractQuote,
    range: ContractRange,
    coins: List<DepositCoin>,
    open: (Chosen, price: ULong) -> Unit,
    dismiss: () -> Unit,
    decline: (() -> Unit)? = null,
    held: ULong = 0uL,
    cashOutFirst: (ULong) -> Unit = {},
) {
    val colors = MaterialTheme.colorScheme
    val download = android.text.format.Formatter.formatShortFileSize(LocalContext.current, q.download.toLong())
    val sizes = remember(range) { contractSizes(range) }
    val start = if (coins.isEmpty()) maxOf(q.suggestedCap, held) else q.suggestedCap
    var blocks by remember(q) { mutableStateOf(q.suggestedBlocks.coerceIn(range.minBlocks, range.maxBlocks)) }
    // At first the largest that pays as suggested, else the largest that
    // pays less: never a cost past the coin to start with.
    val payable = { c: DepositCoin -> payableSize(range, c.value, start, blocks) }
    var coin by remember(q) {
        val suggested = sizeAtMost(sizes, start)
        val paying = coins.firstOrNull { contractLeastCoin(range, suggested, blocks, 0uL) <= it.value }
        mutableStateOf(paying ?: coins.firstOrNull { payable(it) != null } ?: coins.firstOrNull())
    }
    var cap by remember(q) { mutableStateOf(coin?.let(payable) ?: sizeAtMost(sizes, start)) }
    // Her part, as a share of the most her coin funds: all of it at first.
    var part by remember(q) { mutableStateOf(1f) }
    val most = coin?.let { contractMaxCap(range, it.value) ?: range.minCap } ?: range.maxCap
    val choices = sizes.filter { it <= most }.ifEmpty { listOf(range.minCap) }
    val capped = sizeAtMost(choices, cap)
    val value = coin?.value ?: 0uL
    val mostFunded = if (coin == null) 0uL else
        contractMostFunded(range, capped, blocks, value, q.denominations, q.fundedNotes)
    val funded = contractFundable(q.denominations, q.fundedNotes, fundedAt(part, mostFunded))
    val price = contractPrice(range, capped, blocks, funded)
    val fee = contractFee(range, capped, blocks, funded)
    val needed = contractLeastCoin(range, capped, blocks, funded)
    val left = if (value > price + funded) value - price - funded else 0uL
    // It stops protecting a little before it ends.
    val protectsFor = if (blocks > q.afterAssert) blocks - q.afterAssert else 0u
    Sheet("Protect at ${name(q.hub)}", dismiss) {
        Line("Covers up to", sat(capped), tag = "quote-cap")
        if (choices.size > 1) {
            Slider(
                value = choices.indexOf(capped).coerceAtLeast(0).toFloat(),
                onValueChange = { cap = choices[it.roundToInt().coerceIn(0, choices.size - 1)] },
                valueRange = 0f..(choices.size - 1).toFloat(),
                steps = (choices.size - 2).coerceAtLeast(0),
                modifier = Modifier.testTag("contract-cap"),
            )
        }
        Line("For", span(protectsFor.toLong()), tag = "quote-for")
        if (range.maxBlocks > range.minBlocks) {
            Slider(
                value = blocks.toFloat(),
                onValueChange = { blocks = blocksAt(it, range.minBlocks, range.maxBlocks) },
                valueRange = range.minBlocks.toFloat()..range.maxBlocks.toFloat(),
                modifier = Modifier.testTag("contract-blocks"),
            )
        }
        if (mostFunded > 0uL) {
            Line("You fund", sat(funded), tag = "contract-funded")
            Slider(
                value = part,
                onValueChange = { part = it },
                modifier = Modifier.testTag("contract-funding"),
            )
            Text(
                "Paid from your coin and joins your balance once confirmed: the hub charges interest on its own part only.",
                modifier = Modifier.padding(bottom = 8.dp),
                style = MaterialTheme.typography.bodySmall,
                color = colors.onSurfaceVariant,
            )
        }
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
        val first = contractLeastCoin(range, capped, blocks, 0uL)
        val problem = when {
            coin == null -> null
            value < needed -> whyNot(range, capped, value)
            else -> null
        }
        if (coin == null) {
            Say("Paid from Bitcoin you hold, and you hold none yet. Cash out to yourself first, then protect.")
        } else if (problem == null && left >= q.dust) {
            Line("Back to your Bitcoin", "About ${sat(left)}", tag = "contract-change")
        }
        problem?.let {
            Text(
                it,
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
                enabled = problem == null,
            ) {
                open(Chosen(capped, blocks, c.outpoint, funded), price)
            }
        } else {
            BigButton(
                "Cash out ${sat(first)}",
                Modifier.fillMaxWidth().testTag("cash-out-first"),
                enabled = first <= held,
            ) { cashOutFirst(first) }
        }
        decline?.let {
            Spacer(Modifier.height(8.dp))
            BigButton("Not now", Modifier.fillMaxWidth().testTag("decline-offer"), quiet = true, onClick = it)
        }
    }
}

/** What protection asks of its owner, said before it is paid for. */
internal const val DUTY = "Open Licash now and then, and before it ends: what it protects " +
    "is recovered by this app, and is safe after it ends only once cashed out or protected again."

/**
 * Why a coin of `coin` sat cannot pay for protection of up to `cap` sat
 * within `range`: too small for so large a cover (its hub takes a cover
 * of `perCoin` times the coin at most, but for its least), or short of
 * what it costs.
 */
internal fun whyNot(range: ContractRange, cap: ULong, coin: ULong): String {
    val perCoin = range.perCoin.toULong().coerceAtLeast(1uL)
    return if (cap > range.minCap && coin < (cap + perCoin - 1uL) / perCoin) {
        "This coin is too small for so large a cover: cover less, or pay with a larger coin"
    } else {
        "Costs more than this coin holds"
    }
}

/** Of `sizes`, smallest first, the largest up to `cap`: the smallest, if none. */
internal fun sizeAtMost(sizes: List<ULong>, cap: ULong): ULong =
    sizes.lastOrNull { it <= cap } ?: sizes.firstOrNull() ?: cap

/** The part at `position`, from 0 to 1, of a slider up to `most` sat, to the sat. */
internal fun fundedAt(position: Float, most: ULong): ULong = when {
    position <= 0f -> 0uL
    position >= 1f -> most
    else -> (most.toDouble() * position).roundToLong().toULong().coerceAtMost(most)
}

/** The blocks at `value` of a slider from `least` to `most`, in whole days but at its ends. */
internal fun blocksAt(value: Float, least: UInt, most: UInt): UInt {
    val days = (value / DAY_BLOCKS).roundToLong().coerceAtLeast(0L) * DAY_BLOCKS
    return days.toUInt().coerceIn(least, most)
}

/** Blocks in a day, about. */
private const val DAY_BLOCKS = 144L
