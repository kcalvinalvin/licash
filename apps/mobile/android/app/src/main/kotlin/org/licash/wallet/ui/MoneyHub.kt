package org.licash.wallet.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
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
import org.licash.wallet.carriedOn
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
    cashOut: (hub: String, amount: ULong?) -> Unit,
    payOwn: (link: String, address: String) -> Unit,
    fail: (String) -> Unit,
    changed: () -> Unit,
) {
    val scope = rememberCoroutineScope()
    val hub = ui.hubs.find { sameHub(it.url, holding.hub) }
    val now = System.currentTimeMillis() / 1000
    // One closed with its hub still shows while its fold is awaited: her notes are out of her balance.
    val covers = ui.contracts.filter {
        sameHub(it.hub, holding.hub) &&
            (ui.developer || shown(it, now) || ui.recoveries.any { r -> r.id == it.id && carriedOn(r) })
    }
    val opening = ui.opening?.let { sameHub(it, holding.hub) } == true
    // One being set up shows the step it is at in its own row, once it has one.
    val stepping = covers.any { it.state == ContractState.Opening && it.step != null }
    val held = holding.available + holding.pending
    // Protecting: the hub asked what it costs, then what it quotes.
    var quoting by remember { mutableStateOf(false) }
    var quote by remember { mutableStateOf<ContractQuote?>(null) }
    var ending by remember { mutableStateOf<Contract?>(null) }
    var forcing by remember { mutableStateOf<Contract?>(null) }
    var shortfall by remember { mutableStateOf<ULong?>(null) }
    var canceling by remember { mutableStateOf<String?>(null) }
    var cancelingBusy by remember { mutableStateOf(false) }
    var fees by remember { mutableStateOf(false) }
    var moving by remember { mutableStateOf(false) }
    val cover = hubCover(holding.hub, holding, ui.contracts, notes)
    // Not protected and worth a contract: protect it or cash part out, until dismissed.
    var dismissed by remember(holding.hub) { mutableStateOf(engine.prefs.unprotectedDismissed(holding.hub)) }
    val suggest = suggestProtecting(cover, hub, dismissed)

    fun protect() {
        quoting = true
        scope.launch {
            engine.contractQuote(holding.hub).fold({ quote = it }, { fail(explain(it)) })
            quoting = false
        }
    }

    Text(sat(holding.available), modifier = Modifier.padding(horizontal = 4.dp, vertical = 8.dp), style = Heading)
    if (holding.pending > 0uL) Line("Settling", sat(holding.pending), Modifier.padding(horizontal = 4.dp))
    // How much of it its contracts cover, what they leave out, the room they have left.
    cover.says?.let {
        CoverBar(cover, Modifier.padding(horizontal = 4.dp, vertical = 4.dp))
        Text(
            "$it · ${cover.noteWords}",
            modifier = Modifier.padding(horizontal = 4.dp).testTag("hub-cover"),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
    Spacer(Modifier.height(12.dp))
    Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
        BigButton(
            "Cash out",
            Modifier.weight(1f).testTag("cash-out"),
            enabled = holding.available > 0uL,
            quiet = true,
        ) { cashOut(holding.hub, null) }
        if (hub?.contracts != null && ui.opening == null) {
            BigButton("Protect", Modifier.weight(1f).testTag("open-contract"), enabled = !quoting) { protect() }
        }
    }
    if (suggest && ui.opening == null) {
        Group("Not protected") {
            Text(
                unprotectedWords(cover.missing, holding.hub),
                modifier = Modifier.padding(vertical = 12.dp).testTag("unprotected"),
                style = MaterialTheme.typography.bodyMedium,
            )
            Row(Modifier.padding(bottom = 12.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                BigButton("Protect", Modifier.weight(1f).testTag("suggest-protect"), enabled = !quoting) { protect() }
                BigButton("Cash out part", Modifier.weight(1f).testTag("suggest-cash-out"), quiet = true) {
                    cashOut(holding.hub, cover.missing)
                }
            }
            TextButton(onClick = {
                engine.prefs.dismissUnprotected(holding.hub, cover.missing)
                dismissed = cover.missing
            }, Modifier.testTag("suggest-dismiss")) { Text("Not now") }
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
                    ui.network,
                    ui.developer,
                    recovery = ui.recoveries.find { it.id == c.id },
                    safety = { engine.contractSafety(c.id).getOrNull() },
                    inspection = { engine.contractInspection(c.id).getOrNull() },
                    explorer = engine::explorerAddress,
                    end = { ending = c },
                    forceClose = { forcing = c },
                    giveUp = { canceling = c.id },
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
        // Of terms chosen, if the hub lets them be: paid from a coin held
        // that pays within them, or from one cashed out to pay them first.
        val range = q.range
        if (range != null) {
            val cashFirst: (ULong) -> Unit = { amount ->
                quote = null
                scope.launch {
                    engine.bitcoinAddress().fold(
                        { payOwn("bitcoin:$it?amount=${btc(amount)}", it) },
                        { fail(explain(it)) },
                    )
                }
            }
            ChooseSheet(
                q,
                range,
                q.coins.filter { contractMaxCap(range, it.value) != null },
                open = { chosen, price ->
                    quote = null
                    engine.openContract(q.hub, price, chosen)
                },
                dismiss = { quote = null },
                held = holding.available,
                cashOutFirst = cashFirst,
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
                            { payOwn("bitcoin:$it?amount=${btc(q.price)}", it) },
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
                cashOut(c.hub, null)
            },
            close = {
                ending = null
                engine.closeContract(c.id)
            },
            dismiss = { ending = null },
        )
    }
    forcing?.let { c ->
        LaunchedEffect(c.id) { shortfall = engine.challengeShortfall().getOrNull() }
        ForceSheet(
            c,
            held,
            shortfall,
            force = {
                forcing = null
                shortfall = null
                engine.forceClose(c.id)
            },
            dismiss = {
                forcing = null
                shortfall = null
            },
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
