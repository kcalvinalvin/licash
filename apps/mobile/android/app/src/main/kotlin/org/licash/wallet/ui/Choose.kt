package org.licash.wallet.ui

import androidx.compose.foundation.horizontalScroll
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
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.dp
import org.licash.core.ContractFit
import org.licash.core.ContractQuote
import org.licash.core.ContractRange
import org.licash.core.DepositCoin
import org.licash.core.Holding
import org.licash.core.contractFee
import org.licash.core.contractFit
import org.licash.core.contractPrice
import org.licash.wallet.Chosen
import org.licash.wallet.Engine
import org.licash.wallet.digits
import org.licash.wallet.percent
import org.licash.wallet.sat
import kotlin.math.roundToInt
import kotlin.math.roundToLong

/**
 * The protection offered for what was paid on Bitcoin ([org.licash.wallet.Ui.offer]),
 * as [ChooseSheet]: a coin on Bitcoin added to the balance, protected,
 * once accepted, or declined so it is not offered unasked again. None if
 * no coin is taken whole by a contract within its hub's range.
 */
@Composable
internal fun OfferSheet(engine: Engine, q: ContractQuote, ready: Boolean, dismiss: () -> Unit) {
    val range = q.range ?: return
    val fitting = q.coins.filter { fitOf(q, range, q.suggestedBlocks, it) != null }
    if (fitting.isEmpty()) return
    val ui by engine.ui.collectAsState()
    var holding by remember(q.hub) { mutableStateOf<Holding?>(null) }
    LaunchedEffect(q.hub, ui.balance) {
        engine.holdings().onSuccess { all -> holding = all.find { sameHub(it.hub, q.hub) } }
    }
    ChooseSheet(
        q,
        range,
        fitting,
        ready,
        hubCover(q.hub, holding, ui.contracts, emptyList()),
        title = "Add to balance, protected",
        verb = "Add",
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
 * The contracts within `range` over `blocks` that `coin` pays for whole
 * ([contractFit]), one of each cap, smallest first: none if no contract
 * takes the coin. The coin is spent whole, the price and the rest of it her
 * part, issued back to her as notes; the rest of the cap is the hub's.
 */
internal fun fitsOf(q: ContractQuote, range: ContractRange, blocks: UInt, coin: DepositCoin): List<ContractFit> =
    fitsFrom(range.minCap) { least ->
        contractFit(range, blocks, coin.value, least, q.dust, q.denominations, q.fundedNotes)
    }

/** Each contract `fit` gives of `least` sat at least and above, the smallest it finds above the last. */
internal fun fitsFrom(least: ULong, fit: (least: ULong) -> ContractFit?): List<ContractFit> = buildList {
    var from = least
    while (true) {
        val found = fit(from)?.takeIf { it.cap >= from } ?: break
        add(found)
        if (found.cap == ULong.MAX_VALUE) break
        from = found.cap + 1uL
    }
}

/**
 * The contract offered at first of `fits` for a coin of `coin` sat: the
 * smallest covering half again the coin, room to be paid more, else the
 * largest the hub allows for it.
 */
internal fun suggestedFit(fits: List<ContractFit>, coin: ULong): ContractFit? =
    fits.firstOrNull { it.cap >= coin + coin / 2uL } ?: fits.lastOrNull()

/** The contract offered at first for `coin`, if one takes it whole. */
internal fun fitOf(q: ContractQuote, range: ContractRange, blocks: UInt, coin: DepositCoin): ContractFit? =
    suggestedFit(fitsOf(q, range, blocks, coin), coin.value)

/**
 * Each cover some coin of `fits` (each coin's contracts, as [fitsOf])
 * opens whole, smallest first, with the coins paying it, smallest first:
 * the cover is chosen, then a coin that pays it.
 */
internal fun coversOf(fits: List<Pair<DepositCoin, List<ContractFit>>>): List<Pair<ULong, List<DepositCoin>>> =
    fits.flatMap { (coin, its) -> its.map { it.cap to coin } }
        .groupBy({ it.first }, { it.second })
        .toSortedMap()
        .map { (cap, coins) -> cap to coins.sortedBy { it.value } }

/** The cover offered at first: the one suggested for the smallest coin a contract takes whole. */
internal fun suggestedCover(fits: List<Pair<DepositCoin, List<ContractFit>>>): ULong? =
    fits.filter { it.second.isNotEmpty() }.minByOrNull { it.first.value }
        ?.let { (coin, its) -> suggestedFit(its, coin.value)?.cap }

/**
 * Protection of terms chosen within what its hub allows. First what is
 * protected at the hub once it opens (`cover`, the hub's now, with it), then
 * what it covers, chosen first, which decides the coins on Bitcoin that pay
 * it whole: the smallest at first. Then what it costs, the reserve, hers and
 * refundable, and what of the coin joins her balance once its funding
 * confirms: the coin is spent whole. How long, and the cost itemized, under
 * More. Offered unasked, it can be declined. Not paid for until the app is
 * caught up with Bitcoin (`ready`), as opening it needs.
 *
 * With no coin to pay it, she is told to cash out to herself first.
 */
@Composable
internal fun ChooseSheet(
    q: ContractQuote,
    range: ContractRange,
    coins: List<DepositCoin>,
    ready: Boolean,
    cover: HubCover,
    open: (Chosen, price: ULong) -> Unit,
    dismiss: () -> Unit,
    title: String = "Protect at ${name(q.hub)}",
    verb: String = "Protect",
    decline: (() -> Unit)? = null,
) {
    val colors = MaterialTheme.colorScheme
    val download = android.text.format.Formatter.formatShortFileSize(LocalContext.current, q.download.toLong())
    val blockSecs = LocalBlockSeconds.current
    var blocks by remember(q) { mutableStateOf(q.suggestedBlocks.coerceIn(range.minBlocks, range.maxBlocks)) }
    val fits = coins.map { it to fitsOf(q, range, blocks, it) }
    val covers = coversOf(fits)
    // The cover chosen, kept while some coin opens it; at first, or once
    // none does, the one suggested.
    var chosenCap by remember(q) { mutableStateOf<ULong?>(null) }
    var chosenCoin by remember(q) { mutableStateOf<DepositCoin?>(null) }
    val cap = chosenCap?.takeIf { c -> covers.any { it.first == c } } ?: suggestedCover(fits)
    val payers = covers.firstOrNull { it.first == cap }?.second.orEmpty()
    val coin = chosenCoin?.takeIf { it in payers } ?: payers.firstOrNull()
    val fit = coin?.let { c -> fits.first { it.first == c }.second.firstOrNull { it.cap == cap } }
    val capped = fit?.cap ?: range.minCap
    val funded = fit?.funded ?: 0uL
    // What the hub is paid: its fee and interest, then the reserve, hers.
    val price = contractPrice(range, capped, blocks, funded)
    val fee = contractFee(range, capped, blocks, funded)
    val cost = fee.cost()
    // It stops protecting a little before it ends.
    val protectsFor = if (blocks > q.afterAssert) blocks - q.afterAssert else 0u
    Sheet(title, dismiss) {
        if (fit != null) {
            val after = coverAfter(cover, capped, funded)
            CoverBar(after, Modifier.padding(top = 4.dp, bottom = 6.dp))
            after.says?.let {
                Text(
                    "At ${name(q.hub)} then: ${it.replaceFirstChar { c -> c.lowercase() }}",
                    modifier = Modifier.padding(bottom = 8.dp).testTag("contract-after"),
                    style = MaterialTheme.typography.bodySmall,
                    color = colors.onSurfaceVariant,
                )
            }
        }
        Line("Covers up to", sat(capped), tag = "quote-cap")
        // Larger, the hub funds more of it: room to be paid more, at its interest.
        if (fit != null && covers.size > 1) {
            val at = covers.indexOfFirst { it.first == capped }
            Slider(
                value = at.toFloat(),
                onValueChange = { chosenCap = covers[it.roundToInt().coerceIn(covers.indices)].first },
                valueRange = 0f..covers.lastIndex.toFloat(),
                steps = covers.size - 2,
                modifier = Modifier.testTag("contract-cap"),
            )
        }
        if (fit != null && coin != null) {
            if (payers.size > 1) {
                Text("Paid with", modifier = Modifier.padding(top = 8.dp), style = MaterialTheme.typography.bodyLarge)
                Row(Modifier.horizontalScroll(rememberScrollState())) {
                    for (c in payers) {
                        FilterChip(
                            selected = c == coin,
                            onClick = { chosenCoin = c },
                            label = { Text(sat(c.value)) },
                            modifier = Modifier.padding(end = 8.dp).testTag("contract-coin"),
                        )
                    }
                }
            } else {
                Line("Paid with", "${sat(coin.value)} on Bitcoin", tag = "contract-paid-with")
            }
            Row(verticalAlignment = Alignment.CenterVertically) {
                FeeLine(
                    "Cost", cost, capped, tag = "quote-price", value = sat(cost),
                    breakdown = contractCost(range, capped, fee, funded), modifier = Modifier.weight(1f),
                )
                Info("Interest", INTEREST_MEANS)
            }
            Row(verticalAlignment = Alignment.CenterVertically) {
                Line("Reserve, refundable", sat(range.reserve), Modifier.weight(1f), tag = "contract-reserve")
                Info("Reserve", RESERVE_MEANS)
            }
            Line("Into your balance", sat(funded), tag = "contract-funded")
        }
        Line("For", span(protectsFor.toLong(), blockSecs), tag = "quote-for")
        if (coin == null && coins.isEmpty()) {
            Say("Paid from Bitcoin you hold, and you hold none yet. Receive some on Bitcoin, or cash out to yourself, then protect.")
        } else if (fit == null) {
            Text(
                NO_FIT,
                modifier = Modifier.padding(vertical = 8.dp).testTag("contract-problem"),
                style = MaterialTheme.typography.bodyMedium,
                color = colors.error,
            )
        }
        More(modifier = Modifier.testTag("contract-more")) {
            if (range.maxBlocks > range.minBlocks) {
                Text("How long", modifier = Modifier.padding(top = 8.dp), style = MaterialTheme.typography.bodyLarge)
                Slider(
                    value = blocks.toFloat(),
                    onValueChange = { blocks = blocksAt(it, range.minBlocks, range.maxBlocks) },
                    valueRange = range.minBlocks.toFloat()..range.maxBlocks.toFloat(),
                    modifier = Modifier.testTag("contract-blocks"),
                )
            }
            if (fit != null) {
                Line("The hub's part", sat(capped - funded), tag = "contract-hub-part")
                Line("Opening fee", sat(fee.opening), tag = "contract-opening-fee")
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
            }
            Line("Takes", "Up to an hour, $download")
        }
        val now = System.currentTimeMillis() / 1000
        Text(
            duty(day(now + protectsFor.toLong() * blockSecs.toLong(), now)),
            modifier = Modifier.padding(vertical = 8.dp).testTag("contract-duty"),
            style = MaterialTheme.typography.bodySmall,
            color = colors.onSurfaceVariant,
        )
        Worth(q.mock)
        Spacer(Modifier.height(8.dp))
        if (coin != null) {
            BigButton(
                if (ready) "$verb for ${sat(cost)}" else "Waiting for Bitcoin",
                Modifier.fillMaxWidth().testTag("protect"),
                enabled = fit != null && ready,
            ) {
                open(Chosen(capped, blocks, coin.outpoint), price)
            }
        }
        decline?.let {
            Spacer(Modifier.height(8.dp))
            BigButton("Not now", Modifier.fillMaxWidth().testTag("decline-offer"), quiet = true, onClick = it)
        }
    }
}

/** Why no coin pays for a contract: each is spent whole, so short of the price or beyond what the largest takes. */
internal const val NO_FIT = "No contract of this hub takes a coin of yours whole: each is short of the price, " +
    "or beyond what the largest contract takes."

/** What protection asks of its owner, said before it is paid for: its end, `ends`, as a day. */
internal fun duty(ends: String): String = "Open Licash now and then, and before it ends on $ends."

/** The blocks at `value` of a slider from `least` to `most`, in whole days but at its ends. */
internal fun blocksAt(value: Float, least: UInt, most: UInt): UInt {
    val days = (value / DAY_BLOCKS).roundToLong().coerceAtLeast(0L) * DAY_BLOCKS
    return days.toUInt().coerceIn(least, most)
}

/** Blocks in a day, about. */
private const val DAY_BLOCKS = 144L
