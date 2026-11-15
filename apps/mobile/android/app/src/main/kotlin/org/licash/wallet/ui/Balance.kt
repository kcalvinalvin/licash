package org.licash.wallet.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import org.licash.core.Contract
import org.licash.core.ContractQuote
import org.licash.core.ContractState
import org.licash.core.Holding
import org.licash.core.Notes
import org.licash.wallet.Engine
import org.licash.wallet.Ui
import org.licash.wallet.credit
import org.licash.wallet.creditPercent
import org.licash.wallet.digits
import org.licash.wallet.duration
import org.licash.wallet.explain
import org.licash.wallet.host
import org.licash.wallet.sat
import org.licash.wallet.total

/**
 * Where the balance is: in Licash at each hub, with the contracts there,
 * and on Bitcoin; in advanced mode, the notes held of each denomination
 * too. `cashOut` pays all the notes at a hub out on Bitcoin; `payOwn` pays
 * a link to an address of this wallet's, the address given.
 */
@Composable
fun BalanceDetails(
    engine: Engine,
    ui: Ui,
    cashOut: (String) -> Unit,
    payOwn: (link: String, address: String) -> Unit,
    back: () -> Unit,
) {
    var holdings by remember { mutableStateOf<List<Holding>?>(null) }
    var unknown by remember { mutableStateOf(false) }
    var notes by remember { mutableStateOf<List<Notes>>(emptyList()) }
    var visited by remember { mutableStateOf<List<Holding>>(emptyList()) }
    var error by remember { mutableStateOf<String?>(null) }
    // A hub visited the owner asks to add, by its URL.
    var adding by remember { mutableStateOf<String?>(null) }
    var added by remember { mutableStateOf(0) }
    // Opening a contract: the hub asked, then what it quotes.
    var quoting by remember { mutableStateOf<String?>(null) }
    var quote by remember { mutableStateOf<ContractQuote?>(null) }
    var closing by remember { mutableStateOf<Contract?>(null) }
    // Moving notes from a hub to another of the wallet's, in advanced mode.
    var moving by remember { mutableStateOf<Holding?>(null) }
    val scope = rememberCoroutineScope()
    fun askQuote(hub: String) {
        quoting = hub
        scope.launch {
            engine.contractQuote(hub).fold({ quote = it }, { error = explain(it) })
            quoting = null
        }
    }
    LaunchedEffect(ui.balance, ui.history, added) {
        engine.holdings()
            .onSuccess { holdings = it; unknown = false }
            .onFailure { holdings = emptyList(); unknown = true; error = explain(it) }
        engine.notes().onSuccess { notes = it }.onFailure { error = explain(it) }
        engine.visited().onSuccess { visited = it }.onFailure { error = explain(it) }
    }
    val b = ui.balance
    val coming = ui.incoming.sumOf { it.amount }
    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(horizontal = 20.dp)) {
        TopBar("Balance", back)
        Spacer(Modifier.height(12.dp))
        Text(
            sat(b.total),
            modifier = Modifier.padding(horizontal = 4.dp),
            style = MaterialTheme.typography.displaySmall,
        )
        if (b.total > 0uL) Protection(b)
        error?.let { Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall) }
        Group("In Licash") {
            val held = holdings
            when {
                held == null -> Amount("Looking…", null)
                held.isEmpty() -> Amount(if (unknown) "Unknown for now" else "Nothing yet", null)
            }
            held.orEmpty().forEachIndexed { i, holding ->
                if (i > 0) Divider()
                val hub = ui.hubs.find { it.url == holding.hub }
                Amount(host(holding.hub), holding.available, Modifier.testTag("holding"))
                if (holding.pending > 0uL) Amount("Settling", holding.pending)
                hub?.lightningPay?.let { Hint("Pays Lightning invoices of up to ${sat(it)}") }
                hub?.deposits?.let { Hint("Takes Bitcoin in from ${sat(it.min)} to ${sat(it.max)}") }
                val here = ui.contracts.filter { sameHub(it.hub, holding.hub) && (ui.developer || shown(it)) }
                for (c in here) ContractCard(c, ui.chain.height, ui.developer) { closing = c }
                if (ui.opening != null && sameHub(ui.opening, holding.hub)) {
                    Hint("Opening a contract: a few minutes, downloading what it needs.")
                    LinearProgressIndicator(Modifier.fillMaxWidth().padding(bottom = 8.dp))
                }
                // A button that does not fit goes below whole, its words on one line.
                FlowRow {
                    if (holding.available > 0uL) {
                        TextButton(
                            onClick = { cashOut(holding.hub) },
                            modifier = Modifier.testTag("cash-out"),
                        ) { Text("Cash out to Bitcoin", maxLines = 1) }
                    }
                    if (hub?.contracts != null && ui.opening == null) {
                        TextButton(
                            onClick = { askQuote(holding.hub) },
                            enabled = quoting == null,
                            modifier = Modifier.testTag("open-contract"),
                        ) { Text(if (here.any { live(it) }) "Open another contract" else "Open a contract", maxLines = 1) }
                    }
                    if (ui.developer && held.orEmpty().size >= 2 && holding.available > 0uL) {
                        TextButton(
                            onClick = { moving = holding },
                            modifier = Modifier.testTag("move"),
                        ) { Text("Move to another hub", maxLines = 1) }
                    }
                }
            }
        }
        if (visited.isNotEmpty()) {
            Group("At hubs you have not added") {
                Hint("Left over from paying someone at their hub. Not in your balance until you add the hub.")
                visited.forEachIndexed { i, holding ->
                    if (i > 0) Divider()
                    Amount(host(holding.hub), holding.available + holding.pending, Modifier.testTag("visited"))
                    TextButton(onClick = { adding = holding.hub }) { Text("Add this hub") }
                }
            }
        }
        Group("On Bitcoin") {
            val toMove = b.onchain - b.kept
            if (b.kept > 0uL) Amount("Cashed out, kept on Bitcoin", b.kept)
            if (toMove > 0uL) Amount("Confirmed, moved in once enough", toMove)
            if (b.moving > 0uL) Amount("Moving into Licash", b.moving)
            if (b.opening > 0uL) Amount("Coming back from opening a contract", b.opening)
            if (coming > 0uL) Amount("Coming, waiting for a block", coming)
            if (b.onchain == 0uL && b.moving == 0uL && b.opening == 0uL && coming == 0uL) {
                Amount("Nothing now", null)
            }
            if (b.kept > 0uL && b.moving == 0uL) {
                TextButton(
                    onClick = {
                        scope.launch { engine.moveAll().onFailure { error = explain(it) } }
                    },
                ) { Text("Move into Licash") }
            }
        }
        if (ui.developer) {
            Group("Notes held") {
                if (notes.isEmpty()) Amount("None", null)
                notes.groupBy { it.hub }.entries.forEachIndexed { i, (hub, held) ->
                    if (i > 0) Divider()
                    Hint(host(hub))
                    for (n in held) {
                        Amount("${digits(n.denomination)} sat × ${n.count}", n.denomination * n.count, Modifier.testTag("denomination"))
                    }
                }
            }
        }
        Spacer(Modifier.height(48.dp))
    }
    adding?.let { url ->
        AlertDialog(
            onDismissRequest = { adding = null },
            title = { Text("Add this hub?") },
            text = {
                Text(
                    "$url\n\nYou will hold notes there and be paid there, as at your other hubs. " +
                        "Add it only if you trust who runs it.",
                )
            },
            confirmButton = {
                TextButton(onClick = {
                    adding = null
                    scope.launch {
                        engine.addHub(url).onFailure { error = explain(it) }
                        added++
                    }
                }) { Text("Add") }
            },
            dismissButton = { TextButton(onClick = { adding = null }) { Text("Cancel") } },
        )
    }
    quote?.let { q ->
        OpenContract(
            q,
            open = {
                quote = null
                engine.openContract(q.hub, q.price)
            },
            cashOutFirst = {
                quote = null
                // Enough for the price and some change, kept on Bitcoin.
                val amount = q.price + FIRST_CHANGE
                scope.launch {
                    engine.bitcoinAddress().fold(
                        { payOwn("bitcoin:$it?amount=${btc(amount)}", it) },
                        { error = explain(it) },
                    )
                }
            },
            dismiss = { quote = null },
        )
    }
    moving?.let { from ->
        MoveDialog(
            engine,
            from,
            holdings.orEmpty(),
            moved = {
                moving = null
                added++
            },
            dismiss = { moving = null },
        )
    }
    closing?.let { c ->
        val held = holdings.orEmpty().find { sameHub(it.hub, c.hub) }?.available ?: 0uL
        CloseContract(
            c,
            ui.chain.height,
            held,
            cashOut = {
                closing = null
                cashOut(c.hub)
            },
            dismiss = { closing = null },
        )
    }
}

/** What a payout to this wallet leaves as change, opening a contract from it. */
private const val FIRST_CHANGE: ULong = 10_000uL

/** `amount` sat in bitcoin, as a payment link says it. */
private fun btc(amount: ULong): String = "%d.%08d".format(
    java.util.Locale.US,
    (amount / 100_000_000uL).toLong(),
    (amount % 100_000_000uL).toLong(),
)

/** Whether two URLs name one hub: contracts keep it without a last slash. */
private fun sameHub(a: String, b: String): Boolean = a.trimEnd('/') == b.trimEnd('/')

/** What a genuine contract can do for now, said wherever it shows. */
private const val UNRECOVERED = "Genuine programs. Recovery is not in this app yet: treat as unprotected"

/** Protecting, or about to: what a contract is for. */
private fun live(c: Contract): Boolean =
    c.state == ContractState.Opening || c.state == ContractState.Confirming || c.state == ContractState.Protecting

/** Shown but in advanced mode: those not closed, nor failed over a day ago. */
private fun shown(c: Contract): Boolean = when (c.state) {
    ContractState.Closed -> false
    is ContractState.Failed -> System.currentTimeMillis() / 1000 - c.createdAt < 86_400
    else -> true
}

/** A contract at a hub: where it stands, what it protects and until when. */
@Composable
private fun ContractCard(c: Contract, height: Int, developer: Boolean, close: () -> Unit) {
    val colors = MaterialTheme.colorScheme
    val (label, tint) = when (c.state) {
        ContractState.Opening -> "Opening" to colors.onSurfaceVariant
        ContractState.Confirming -> "Confirming" to colors.onSurfaceVariant
        // A genuine one protects nothing this app can claim yet: live only.
        ContractState.Protecting -> (if (c.mock) "Protecting" else "Live") to colors.primary
        ContractState.Ending -> "Ending" to colors.onSurfaceVariant
        ContractState.Closed -> "Closed" to colors.onSurfaceVariant
        is ContractState.Failed -> "Failed" to colors.error
    }
    Column(
        Modifier.fillMaxWidth().padding(vertical = 6.dp).clip(RoundedCornerShape(12.dp))
            .background(colors.surfaceContainerHigh).padding(horizontal = 14.dp, vertical = 10.dp)
            .testTag("contract"),
    ) {
        Row(Modifier.fillMaxWidth()) {
            Text("Contract", Modifier.weight(1f), style = MaterialTheme.typography.titleSmall)
            Text(label, Modifier.testTag("contract-state"), style = MaterialTheme.typography.labelLarge, color = tint)
        }
        Text(
            if (c.mock) "Mock, for testing: it protects nothing" else UNRECOVERED,
            style = MaterialTheme.typography.bodySmall,
            color = colors.error,
        )
        Spacer(Modifier.height(4.dp))
        val left = c.protectsUntil.toLong() - height
        when (val s = c.state) {
            is ContractState.Failed -> Fact("Why", s.reason)
            ContractState.Ending, ContractState.Closed ->
                Fact(if (c.mock) "Protected until" else "Covered until", "block ${c.protectsUntil}")
            else -> {
                Fact(if (c.mock) "Protects up to" else "Covers up to", sat(c.cap))
                Fact("Until", if (height > 0) "block ${c.protectsUntil}, ${duration(left)}" else "block ${c.protectsUntil}")
            }
        }
        if (c.state != ContractState.Closed && c.state !is ContractState.Failed) {
            val closes = c.closesAt.toLong() - height
            Fact("Closes", if (height > 0) "at block ${c.closesAt}, ${duration(closes)}" else "at block ${c.closesAt}")
        }
        if (c.state !is ContractState.Failed) {
            Fact("Cost", sat(c.price))
            Fact("Paid from", "A coin of ${sat(c.paidFrom)}")
        }
        if (developer) {
            c.funding?.let { Fact("Funding", it.take(16) + "…") }
            Fact("Id", c.id.take(16) + "…")
        }
        val open = c.state == ContractState.Confirming || c.state == ContractState.Protecting ||
            c.state == ContractState.Ending
        if (open) {
            Row {
                TextButton(onClick = close, modifier = Modifier.testTag("close-contract")) { Text("Close") }
                if (developer) TextButton(onClick = {}, enabled = false) { Text("Force close") }
            }
            if (developer) Hint("Force closing, recovering your notes on Bitcoin, is not available yet.")
        }
    }
}

@Composable
private fun Fact(label: String, value: String) {
    Row(Modifier.fillMaxWidth().padding(vertical = 2.dp)) {
        Text(
            label,
            Modifier.width(112.dp),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Text(value, Modifier.weight(1f), style = MaterialTheme.typography.bodySmall)
    }
}

/** What opening a contract costs and does, to open it, or to get a coin to pay it first. */
@Composable
private fun OpenContract(q: ContractQuote, open: () -> Unit, cashOutFirst: () -> Unit, dismiss: () -> Unit) {
    val context = LocalContext.current
    val download = android.text.format.Formatter.formatShortFileSize(context, q.download.toLong())
    val coin = q.coin
    AlertDialog(
        onDismissRequest = dismiss,
        title = { Text("Open a contract at ${host(q.hub)}?") },
        text = {
            Column {
                val lasts = "${q.protectsFor} blocks, ${duration(q.protectsFor.toLong())}, and closes by itself after."
                Text(
                    if (q.mock) {
                        "The hub locks collateral on Bitcoin behind your notes there, up to ${sat(q.cap)}: " +
                            "if it fails them, you recover them from it. It protects for $lasts"
                    } else {
                        "The hub locks collateral on Bitcoin behind your notes there, up to ${sat(q.cap)}, " +
                            "for $lasts"
                    },
                )
                Spacer(Modifier.height(8.dp))
                if (coin != null) {
                    Text(
                        "It costs ${sat(q.price)}, paid from a coin of yours on Bitcoin of ${sat(coin)}: " +
                            "the rest comes back to this wallet.",
                    )
                } else {
                    Text(
                        "It costs ${sat(q.price)}, paid from a coin of yours on Bitcoin, and you have none " +
                            "that pays it. Cash out ${sat(q.price + FIRST_CHANGE)} to this wallet first: " +
                            "it stays on Bitcoin, and you open the contract from it once it confirms.",
                    )
                }
                Spacer(Modifier.height(8.dp))
                Text(
                    "Opening takes a few minutes and downloads about $download.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(8.dp))
                Text(
                    if (q.mock) "A mock contract, for testing: it protects nothing." else "$UNRECOVERED.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.error,
                )
            }
        },
        confirmButton = {
            if (coin != null) {
                TextButton(onClick = open) { Text("Open for ${sat(q.price)}") }
            } else {
                TextButton(onClick = cashOutFirst) { Text("Cash out to this wallet") }
            }
        },
        dismissButton = { TextButton(onClick = dismiss) { Text("Cancel") } },
    )
}

/** Why a contract does not close at once, and what to do instead: cash out its hub's notes. */
@Composable
private fun CloseContract(c: Contract, height: Int, held: ULong, cashOut: () -> Unit, dismiss: () -> Unit) {
    val closes = if (height > 0) ", ${duration(c.closesAt.toLong() - height)}" else ""
    AlertDialog(
        onDismissRequest = dismiss,
        title = { Text("Close this contract?") },
        text = {
            Text(
                "A contract closes by itself at block ${c.closesAt}$closes, when the hub's collateral " +
                    "returns to it, and cannot close sooner. To stop relying on ${host(c.hub)}, cash out " +
                    "what you hold there to this wallet: " +
                    if (held > 0uL) "${sat(held)} now." else "you hold nothing there now.",
            )
        },
        confirmButton = {
            if (held > 0uL) {
                TextButton(onClick = cashOut) { Text("Cash out") }
            } else {
                TextButton(onClick = dismiss) { Text("OK") }
            }
        },
        dismissButton = { if (held > 0uL) TextButton(onClick = dismiss) { Text("Cancel") } },
    )
}

/** How much is the wallet's own, on Bitcoin, and how much hubs owe on credit. */
@Composable
private fun Protection(b: org.licash.core.Balance) {
    val credit = b.creditPercent
    val owned = 100 - credit
    val safe = MaterialTheme.colorScheme.primary
    val owed = MaterialTheme.colorScheme.error
    Spacer(Modifier.height(12.dp))
    Row(
        Modifier.fillMaxWidth().height(10.dp).clip(RoundedCornerShape(5.dp))
            .background(MaterialTheme.colorScheme.surfaceContainerHighest),
    ) {
        if (owned > 0) Box(Modifier.weight(owned.toFloat()).fillMaxHeight().background(safe))
        if (credit > 0) Box(Modifier.weight(credit.toFloat()).fillMaxHeight().background(owed))
    }
    Spacer(Modifier.height(8.dp))
    Row(Modifier.fillMaxWidth()) {
        Text(
            "$owned% protected",
            modifier = Modifier.weight(1f).testTag("protected"),
            style = MaterialTheme.typography.labelLarge,
            color = safe,
        )
        Text(
            "$credit% on credit, ${sat(b.credit)}",
            modifier = Modifier.testTag("credit"),
            style = MaterialTheme.typography.labelLarge,
            color = owed,
        )
    }
    Text(
        "Protected: on Bitcoin, yours alone, or notes a contract covers. On credit: notes your " +
            "hubs owe you that no contract covers, worth what the hub that issued them is.",
        modifier = Modifier.padding(top = 4.dp),
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )
}

@Composable
private fun Amount(label: String, amount: ULong?, modifier: Modifier = Modifier) {
    Row(modifier.fillMaxWidth().padding(vertical = 12.dp)) {
        Text(label, Modifier.weight(1f), style = MaterialTheme.typography.bodyLarge)
        Spacer(Modifier.width(12.dp))
        amount?.let {
            Text(
                sat(it),
                style = MaterialTheme.typography.bodyLarge,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                textAlign = TextAlign.End,
            )
        }
    }
}

@Composable
private fun Hint(text: String) {
    Text(
        text,
        modifier = Modifier.padding(bottom = 8.dp),
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )
}
