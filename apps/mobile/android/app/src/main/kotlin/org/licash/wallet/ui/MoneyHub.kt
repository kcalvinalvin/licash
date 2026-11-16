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
import androidx.compose.runtime.collectAsState
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
import org.licash.core.ContractState
import org.licash.core.Holding
import org.licash.core.Notes
import org.licash.core.contractMaxCap
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
    // One being set up shows the step it is at in its own row, once it has one.
    val stepping = covers.any { it.state == ContractState.Opening && it.step != null }
    val held = holding.available + holding.pending
    // Protecting: the hub asked what it costs, then what it quotes.
    var quoting by remember { mutableStateOf(false) }
    var quote by remember { mutableStateOf<ContractQuote?>(null) }
    var ending by remember { mutableStateOf<Contract?>(null) }
    var forcing by remember { mutableStateOf<Contract?>(null) }
    var canceling by remember { mutableStateOf<String?>(null) }
    var cancelingBusy by remember { mutableStateOf(false) }
    val proofs by engine.prover.proofs.collectAsState()
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
            // Of what is held here, how much those running protect.
            if (covers.any { it.state == ContractState.Protecting }) {
                Line("Protected now", "${digits(covers.sumOf { it.covers })} of ${sat(held)}", tag = "hub-covered")
                Divider()
            }
            covers.forEachIndexed { i, c ->
                if (i > 0) Divider()
                CoverRow(
                    c,
                    ui.chain.height,
                    ui.developer,
                    recovery = ui.recoveries.find { it.id == c.id },
                    safety = { engine.contractSafety(c.id).getOrNull() },
                    end = { ending = c },
                    forceClose = { forcing = c },
                    giveUp = { canceling = c.id },
                    saveStatement = { to ->
                        scope.launch { engine.saveStatement(c.id, to).onFailure { fail(explain(it)) } }
                    },
                    bringProof = { from -> engine.bringProof(c.id, from) },
                    proving = proofs[c.id],
                    proveHere = if (engine.prover.available) {
                        { scope.launch { engine.proveHere(c.id).onFailure { fail(explain(it)) } } }
                    } else {
                        null
                    },
                    stopProving = { engine.stopProving() },
                    saveReceipt = { to ->
                        scope.launch { engine.saveReceipt(c.id, to).onFailure { fail(explain(it)) } }
                    },
                )
            }
            if (opening && !stepping) {
                if (covers.isNotEmpty()) Divider()
                Entry("Setting up", sub = "Up to an hour", tag = "opening")
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
        // Of terms chosen, if the hub lets them be and a coin held pays within them.
        val range = q.range
        val paying = q.coins.filter { range != null && contractMaxCap(range, it.value) != null }
        if (range != null && paying.isNotEmpty()) {
            ChooseSheet(
                q,
                range,
                paying,
                open = { chosen, price ->
                    quote = null
                    engine.openContract(q.hub, price, chosen)
                },
                dismiss = { quote = null },
            )
        } else {
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
    forcing?.let { c ->
        ForceSheet(
            c,
            held,
            force = {
                forcing = null
                engine.forceClose(c.id)
            },
            dismiss = { forcing = null },
        )
    }
    canceling?.let { id ->
        GiveUpSheet(
            recovery = ui.recoveries.find { it.id == id },
            busy = cancelingBusy,
            confirm = {
                cancelingBusy = true
                scope.launch {
                    engine.giveUpRecovery(id).fold(
                        { canceling = null },
                        { canceling = null; fail("Couldn't cancel closing: ${explain(it)}") },
                    )
                    cancelingBusy = false
                }
            },
            dismiss = { if (!cancelingBusy) canceling = null },
        )
    }
    if (moving) {
        MoveSheet(
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
