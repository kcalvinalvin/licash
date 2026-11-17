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
import org.licash.core.ChangeProblem
import org.licash.core.ChangeRange
import org.licash.core.ContractQuote
import org.licash.core.ContractRange
import org.licash.core.DepositCoin
import org.licash.core.contractChangeProblem
import org.licash.core.contractMaxCap
import org.licash.core.contractPrice
import org.licash.wallet.Chosen
import org.licash.wallet.Engine
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
 * Protection of terms chosen within what its hub allows: how much it
 * covers and for how long, at first as `q` suggests, priced as the hub
 * prices them, and which of `coins` pays, if there are several. A coin
 * turns whole into the protection and notes it covers: what is left of
 * it joins the balance, never left as change on Bitcoin, so it covers
 * that at least; a coin that cannot is refused, said why
 * ([contractChangeProblem]). Offered unasked, it can be declined.
 *
 * With no coin to pay it, the terms are chosen all the same, at first
 * covering the `held` notes, and [coinFor] them is cashed out first
 * (`cashOutFirst`), if `held` pays it.
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
    val start = if (coins.isEmpty()) maxOf(q.suggestedCap, held) else q.suggestedCap
    var blocks by remember(q) { mutableStateOf(q.suggestedBlocks.coerceIn(range.minBlocks, range.maxBlocks)) }
    // At first the largest that pays as suggested, whole.
    var coin by remember(q) {
        val price = contractPrice(range, start, blocks)
        val whole = coins.firstOrNull { contractChangeProblem(it.value, price, start, q.dust, q.change) == null }
        mutableStateOf(whole ?: coins.firstOrNull())
    }
    var cap by remember(q) { mutableStateOf(start) }
    val most = coin?.let { contractMaxCap(range, it.value) ?: range.minCap } ?: range.maxCap
    val value = coin?.value ?: 0uL
    // As asked, what is left of the coin it covers at least: it costs more then, leaving less.
    val asked = contractPrice(range, cap.coerceIn(range.minCap, most), blocks)
    val least = coverAtLeast(range.minCap, value, asked).coerceAtMost(most)
    val capped = cap.coerceIn(least, most)
    val price = contractPrice(range, capped, blocks)
    val change = if (value > price) value - price else 0uL
    // It stops protecting a little before it ends.
    val protectsFor = if (blocks > q.afterAssert) blocks - q.afterAssert else 0u
    Sheet("Protect at ${name(q.hub)}", dismiss) {
        Line("Covers up to", sat(capped), tag = "quote-cap")
        if (most > least) {
            Slider(
                value = capPosition(capped, least, most),
                onValueChange = { cap = capAt(it, least, most) },
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
        val first = coinFor(range, capped, price, q.dust, q.change)
        val paying = coin?.value ?: first
        val problem = contractChangeProblem(paying, price, capped, q.dust, q.change)
        if (coin == null) {
            Say("Paid from Bitcoin you hold, and you hold none yet. Cash out to yourself first, then protect.")
        } else if (problem == null && change >= q.dust) {
            Line("Joins your balance, covered", "About ${sat(change)}", tag = "contract-change")
        }
        problem?.let {
            Text(
                whyNot(it),
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
                open(Chosen(capped, blocks, c.outpoint), price)
            }
        } else {
            BigButton(
                "Cash out ${sat(first)}",
                Modifier.fillMaxWidth().testTag("cash-out-first"),
                enabled = first <= held && problem == null,
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
 * The least protection paid from a coin of `coin` sat at about `price`
 * covers: what is left of the coin once paid, which joins the balance,
 * and the hub's least, `minCap`.
 */
internal fun coverAtLeast(minCap: ULong, coin: ULong, price: ULong): ULong =
    maxOf(minCap, if (coin > price) coin - price else 0uL)

/** Why a coin cannot pay for protection, turned whole into it and the notes it covers. */
internal fun whyNot(p: ChangeProblem): String = when (p) {
    is ChangeProblem.Short -> "Costs more than this coin holds"
    is ChangeProblem.Uncovered ->
        "The ${sat(p.change)} left of this coin would join your balance uncovered: cover more"
    is ChangeProblem.NotTaken ->
        "This hub takes nothing left of a coin into your balance: pay with a coin of the cost"
    is ChangeProblem.TooLittle ->
        "The ${sat(p.change)} left of this coin is less than the hub takes into your balance " +
            "(${sat(p.least)}): choose another coin or other terms"
    is ChangeProblem.TooMuch ->
        "The ${sat(p.change)} left of this coin is more than the hub takes at once " +
            "(${sat(p.most)}): pay with a smaller coin"
}

/**
 * The coin to cash out to this wallet to pay, within `range`, `price`
 * for protection of up to `cap` sat, turned whole into it and notes: one
 * the hub takes as paying for so large a cap, and the price, whichever is
 * more; if what is left of it is less than the hub takes into notes
 * (`change`), and more than its funding's `dust`, as much more as that,
 * should the cap still cover it. Otherwise no coin pays for so small a
 * cap ([contractChangeProblem] says why).
 */
internal fun coinFor(range: ContractRange, cap: ULong, price: ULong, dust: ULong, change: ChangeRange?): ULong {
    val perCoin = range.perCoin.toULong().coerceAtLeast(1uL)
    val coin = maxOf((cap + perCoin - 1uL) / perCoin, price)
    val left = coin - price
    val bump = change != null && left >= dust && left < change.least && change.least <= minOf(cap, change.most)
    return if (bump) price + change.least else coin
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
