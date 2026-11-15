package org.licash.wallet.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
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
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import org.licash.core.Holding
import org.licash.core.Notes
import org.licash.wallet.Engine
import org.licash.wallet.Ui
import org.licash.wallet.digits
import org.licash.wallet.explain
import org.licash.wallet.host
import org.licash.wallet.sat

/**
 * Where the balance is: in Licash at each hub, and on Bitcoin; in advanced
 * mode, the notes held of each denomination too. `cashOut` pays all the
 * notes at a hub out on Bitcoin.
 */
@Composable
fun BalanceDetails(engine: Engine, ui: Ui, cashOut: (String) -> Unit, back: () -> Unit) {
    var holdings by remember { mutableStateOf<List<Holding>?>(null) }
    var notes by remember { mutableStateOf<List<Notes>>(emptyList()) }
    var visited by remember { mutableStateOf<List<Holding>>(emptyList()) }
    var error by remember { mutableStateOf<String?>(null) }
    // A hub visited the owner asks to add, by its URL.
    var adding by remember { mutableStateOf<String?>(null) }
    var added by remember { mutableStateOf(0) }
    val scope = rememberCoroutineScope()
    LaunchedEffect(ui.balance, ui.history, added) {
        engine.holdings().onSuccess { holdings = it }.onFailure { holdings = emptyList(); error = explain(it) }
        engine.notes().onSuccess { notes = it }.onFailure { error = explain(it) }
        engine.visited().onSuccess { visited = it }.onFailure { error = explain(it) }
    }
    val b = ui.balance
    val coming = ui.incoming.sumOf { it.amount }
    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(horizontal = 20.dp)) {
        TopBar("Balance", back)
        Spacer(Modifier.height(12.dp))
        Text(
            sat(b.notes + b.notesPending + b.onchain + b.moving),
            modifier = Modifier.padding(horizontal = 4.dp),
            style = MaterialTheme.typography.displaySmall,
        )
        error?.let { Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall) }
        Group("In Licash") {
            val held = holdings
            when {
                held == null -> Amount("Looking…", null)
                held.isEmpty() -> Amount("Nothing yet", null)
            }
            held.orEmpty().forEachIndexed { i, holding ->
                if (i > 0) Divider()
                val hub = ui.hubs.find { it.url == holding.hub }
                Amount(host(holding.hub), holding.available, Modifier.testTag("holding"))
                if (holding.pending > 0uL) Amount("Settling", holding.pending)
                hub?.lightningPay?.let { Hint("Pays Lightning invoices of up to ${sat(it)}") }
                hub?.deposits?.let { Hint("Takes Bitcoin in from ${sat(it.min)} to ${sat(it.max)}") }
                if (holding.available > 0uL) {
                    TextButton(
                        onClick = { cashOut(holding.hub) },
                        modifier = Modifier.testTag("cash-out"),
                    ) { Text("Cash out to Bitcoin") }
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
            Amount("Confirmed, moved in once enough", b.onchain)
            if (b.moving > 0uL) Amount("Moving into Licash", b.moving)
            if (coming > 0uL) Amount("Coming, waiting for a block", coming)
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
