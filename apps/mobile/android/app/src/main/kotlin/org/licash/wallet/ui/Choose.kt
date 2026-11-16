package org.licash.wallet.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.dp
import org.licash.core.ContractQuote
import org.licash.core.ContractRange
import org.licash.core.DepositCoin
import org.licash.core.contractMaxCap
import org.licash.core.contractPrice
import org.licash.wallet.Chosen
import org.licash.wallet.sat
import org.licash.wallet.share
import kotlin.math.exp
import kotlin.math.floor
import kotlin.math.ln
import kotlin.math.log10
import kotlin.math.max
import kotlin.math.pow
import kotlin.math.roundToLong

/**
 * Protection of terms chosen within what its hub allows: how much it
 * covers and for how long, at first as `q` suggests, priced as the hub
 * prices them; which of `coins` pays, if there are several, and whether
 * what is left of it joins the balance once paid.
 */
@Composable
internal fun ChooseSheet(
    q: ContractQuote,
    range: ContractRange,
    coins: List<DepositCoin>,
    open: (Chosen, price: ULong) -> Unit,
    dismiss: () -> Unit,
) {
    val colors = MaterialTheme.colorScheme
    val download = android.text.format.Formatter.formatShortFileSize(LocalContext.current, q.download.toLong())
    var coin by remember(q) { mutableStateOf(coins.first()) }
    var cap by remember(q) { mutableStateOf(q.suggestedCap) }
    var blocks by remember(q) { mutableStateOf(q.suggestedBlocks.coerceIn(range.minBlocks, range.maxBlocks)) }
    var moveChange by remember(q) { mutableStateOf(true) }
    val most = contractMaxCap(range, coin.value) ?: range.minCap
    val capped = cap.coerceIn(range.minCap, most)
    val price = contractPrice(range, capped, blocks)
    val pays = coin.value > price
    val change = if (pays) coin.value - price else 0uL
    // It stops protecting a little before it ends.
    val protectsFor = if (blocks > q.afterAssert) blocks - q.afterAssert else 0u
    Sheet("Protect at ${name(q.hub)}", dismiss) {
        Line("Covers up to", sat(capped), tag = "quote-cap")
        if (most > range.minCap) {
            Slider(
                value = capPosition(capped, range.minCap, most),
                onValueChange = { cap = capAt(it, range.minCap, most) },
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
        FeeLine("Cost", price, capped, tag = "quote-price", value = sat(price), breakdown = setupCost(capped, price))
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
        if (pays) {
            Row(
                Modifier.fillMaxWidth().clickable { moveChange = !moveChange }.heightIn(min = 56.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Column(Modifier.weight(1f)) {
                    Text("Add the rest to your balance", style = MaterialTheme.typography.bodyLarge)
                    Text(
                        "About ${sat(change)}",
                        modifier = Modifier.testTag("contract-change"),
                        style = MaterialTheme.typography.bodySmall,
                        color = colors.onSurfaceVariant,
                    )
                }
                Switch(moveChange, { moveChange = it }, Modifier.testTag("contract-move-change"))
            }
            // Past what it covers: said before it is paid for.
            if (moveChange && change > capped) {
                Text(
                    "${sat(change - capped)} of it will not be protected",
                    modifier = Modifier.padding(bottom = 8.dp).testTag("contract-over"),
                    style = MaterialTheme.typography.bodySmall,
                    color = colors.error,
                )
            }
        } else {
            Text(
                "Costs more than this coin holds",
                modifier = Modifier.padding(vertical = 12.dp),
                style = MaterialTheme.typography.bodyLarge,
                color = colors.error,
            )
        }
        Line("Takes", "Up to an hour, $download")
        Worth(q.mock)
        Spacer(Modifier.height(8.dp))
        BigButton("Protect for ${sat(price)}", Modifier.fillMaxWidth().testTag("protect"), enabled = pays) {
            open(Chosen(capped, blocks, coin.outpoint, pays && moveChange), price)
        }
    }
}

/**
 * The cap at `position`, from 0 to 1, of a slider from `least` to `most`
 * sat, evenly by ratio (so a small cap is as easy to pick as a large one),
 * to two significant digits but at its ends.
 */
internal fun capAt(position: Float, least: ULong, most: ULong): ULong {
    if (most <= least || position <= 0f) return least
    if (position >= 1f) return most
    val lo = ln(least.coerceAtLeast(1uL).toDouble())
    val exact = exp(lo + position * (ln(most.toDouble()) - lo))
    val unit = 10.0.pow(max(0.0, floor(log10(exact)) - 1))
    return ((exact / unit).roundToLong() * unit).roundToLong().toULong().coerceIn(least, most)
}

/** Where `cap` is on [capAt]'s slider. */
internal fun capPosition(cap: ULong, least: ULong, most: ULong): Float {
    if (most <= least) return 0f
    val lo = ln(least.coerceAtLeast(1uL).toDouble())
    return ((ln(cap.coerceAtLeast(1uL).toDouble()) - lo) / (ln(most.toDouble()) - lo)).toFloat().coerceIn(0f, 1f)
}

/** The blocks at `value` of a slider from `least` to `most`, in whole days but at its ends. */
internal fun blocksAt(value: Float, least: UInt, most: UInt): UInt {
    val days = (value / DAY_BLOCKS).roundToLong().coerceAtLeast(0L) * DAY_BLOCKS
    return days.toUInt().coerceIn(least, most)
}

/** Blocks in a day, about. */
private const val DAY_BLOCKS = 144L
