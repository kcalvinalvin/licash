package org.licash.wallet.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import org.licash.core.Contract
import org.licash.core.ContractQuote
import org.licash.core.Holding
import org.licash.core.Notes
import org.licash.wallet.Engine
import org.licash.wallet.Ui
import org.licash.wallet.digits
import org.licash.wallet.explain
import org.licash.wallet.sat

/**
 * What is held at one hub, and what to do with it: cash it out, protect
 * it. Its protections, each opening on a tap; what the hub charges, once
 * asked. In advanced mode, the notes held there and moving them to
 * another hub too.
 */
@Composable
internal fun HubPage(
    engine: Engine,
    ui: Ui,
    holding: Holding,
    others: List<Holding>,
    notes: List<Notes>,
    cashOut: (String) -> Unit,
    payOwn: (link: String, address: String) -> Unit,
    fail: (String) -> Unit,
    changed: () -> Unit,
) {
    val scope = rememberCoroutineScope()
    val hub = ui.hubs.find { sameHub(it.url, holding.hub) }
    val now = System.currentTimeMillis() / 1000
    val covers = ui.contracts.filter { sameHub(it.hub, holding.hub) && (ui.developer || shown(it, now)) }
    val opening = ui.opening?.let { sameHub(it, holding.hub) } == true
    // Protecting: the hub asked what it costs, then what it quotes.
    var quoting by remember { mutableStateOf(false) }
    var quote by remember { mutableStateOf<ContractQuote?>(null) }
    var ending by remember { mutableStateOf<Contract?>(null) }
    var fees by remember { mutableStateOf(false) }
    var moving by remember { mutableStateOf(false) }

    Text(sat(holding.available), modifier = Modifier.padding(horizontal = 4.dp, vertical = 8.dp), style = Heading)
    if (holding.pending > 0uL) Line("Settling", sat(holding.pending), Modifier.padding(horizontal = 4.dp))
    Spacer(Modifier.height(12.dp))
    Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
        BigButton(
            "Cash out",
            Modifier.weight(1f).testTag("cash-out"),
            enabled = holding.available > 0uL,
            quiet = true,
        ) { cashOut(holding.hub) }
        if (hub?.contracts != null && ui.opening == null) {
            BigButton("Protect", Modifier.weight(1f).testTag("open-contract"), enabled = !quoting) {
                quoting = true
                scope.launch {
                    engine.contractQuote(holding.hub).fold({ quote = it }, { fail(explain(it)) })
                    quoting = false
                }
            }
        }
    }
    if (covers.isNotEmpty() || opening) {
        Group("Protection") {
            covers.forEachIndexed { i, c ->
                if (i > 0) Divider()
                CoverRow(c, ui.chain.height, ui.developer) { ending = c }
            }
            if (opening) {
                if (covers.isNotEmpty()) Divider()
                Entry("Setting up", sub = "A few minutes", tag = "opening")
                LinearProgressIndicator(Modifier.fillMaxWidth().padding(bottom = 12.dp))
            }
        }
    }
    Group("") {
        Entry("Fees and limits", onClick = { fees = true })
        if (ui.developer && others.size >= 2 && holding.available > 0uL) {
            Divider()
            Entry("Move to another hub", modifier = Modifier.testTag("move"), onClick = { moving = true })
        }
    }
    if (ui.developer) {
        Group("Notes") {
            if (notes.isEmpty()) Entry("None")
            notes.forEachIndexed { i, n ->
                if (i > 0) Divider()
                Line("${digits(n.denomination)} sat × ${n.count}", sat(n.denomination * n.count), tag = "denomination")
            }
        }
    }

    if (fees && hub != null) HubFees(hub) { fees = false }
    quote?.let { q ->
        ProtectSheet(
            q,
            open = {
                quote = null
                engine.openContract(q.hub, q.price)
            },
            cashOutFirst = {
                quote = null
                scope.launch {
                    engine.bitcoinAddress().fold(
                        { payOwn("bitcoin:$it?amount=${btc(firstCoin(q.price))}", it) },
                        { fail(explain(it)) },
                    )
                }
            },
            dismiss = { quote = null },
        )
    }
    ending?.let { c ->
        EndSheet(
            c,
            ui.chain.height,
            holding.available,
            cashOut = {
                ending = null
                cashOut(c.hub)
            },
            dismiss = { ending = null },
        )
    }
    if (moving) {
        MoveDialog(
            engine,
            holding,
            others,
            moved = {
                moving = false
                changed()
            },
            dismiss = { moving = false },
        )
    }
}
