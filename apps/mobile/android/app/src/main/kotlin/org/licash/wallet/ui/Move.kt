package org.licash.wallet.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.testTagsAsResourceId
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import org.licash.core.Holding
import org.licash.core.MoveQuote
import org.licash.wallet.Engine
import org.licash.wallet.explain
import org.licash.wallet.host
import org.licash.wallet.sat

/**
 * Move notes from `from` to another of the wallet's hubs, through a swap
 * provider: the hub to move to and the amount (all, if none), then the
 * providers' offers to choose from, the cheapest first. If the chosen one
 * fails before locking the notes, the next costing no more is tried.
 */
@Composable
fun MoveDialog(engine: Engine, from: Holding, hubs: List<Holding>, moved: () -> Unit, dismiss: () -> Unit) {
    val others = hubs.filter { it.hub != from.hub }
    var to by remember { mutableStateOf(others.firstOrNull()?.hub) }
    var text by remember { mutableStateOf("") }
    var offers by remember { mutableStateOf<List<MoveQuote>?>(null) }
    var chosen by remember { mutableStateOf(0) }
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    val scope = rememberCoroutineScope()
    val shown = offers
    AlertDialog(
        onDismissRequest = { if (!busy) dismiss() },
        modifier = Modifier.semantics { testTagsAsResourceId = true },
        title = { Text("Move from ${host(from.hub)}") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState())) {
                Text(
                    "A swap provider takes your notes here and gives you as much at the other hub, " +
                        "less its fee. If it never does, your notes come back once the lock expires.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(12.dp))
                if (shown == null) {
                    Text("To", style = MaterialTheme.typography.titleSmall)
                    for (other in others) {
                        Row(
                            Modifier.fillMaxWidth().clickable { to = other.hub }.padding(vertical = 2.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            RadioButton(selected = to == other.hub, onClick = { to = other.hub })
                            Text("${host(other.hub)}, ${sat(other.available)} now")
                        }
                    }
                    OutlinedTextField(
                        value = text,
                        onValueChange = { text = it.filter(Char::isDigit).take(15) },
                        modifier = Modifier.fillMaxWidth().testTag("move-amount"),
                        label = { Text("Amount in sat, all if empty") },
                        singleLine = true,
                        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                    )
                    Text(
                        "You hold ${sat(from.available)} here.",
                        Modifier.padding(top = 4.dp),
                        style = MaterialTheme.typography.bodySmall,
                    )
                } else {
                    Text("Offers", style = MaterialTheme.typography.titleSmall)
                    shown.forEachIndexed { i, o ->
                        Row(
                            Modifier.fillMaxWidth().clickable { chosen = i }.padding(vertical = 4.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            RadioButton(selected = chosen == i, onClick = { chosen = i })
                            Spacer(Modifier.width(4.dp))
                            Column {
                                Text("${sat(o.amount)} reach ${host(o.to)}")
                                Text(
                                    "Fees ${sat(o.fee)}, through ${o.providerName ?: host(o.provider)}",
                                    style = MaterialTheme.typography.bodySmall,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                )
                            }
                        }
                    }
                }
                if (busy) LinearProgressIndicator(Modifier.fillMaxWidth().padding(top = 12.dp))
                error?.let {
                    Text(it, Modifier.padding(top = 8.dp), color = MaterialTheme.colorScheme.error)
                }
            }
        },
        confirmButton = {
            val target = to
            TextButton(
                enabled = !busy && target != null,
                modifier = Modifier.testTag("move-go"),
                onClick = {
                    busy = true
                    error = null
                    scope.launch {
                        if (shown == null && target != null) {
                            val amount = text.toULongOrNull()?.takeIf { it > 0uL }
                            engine.moveOffers(from.hub, target, amount)
                                .onSuccess { offers = it; chosen = 0 }
                                .onFailure { error = explain(it) }
                        } else if (shown != null) {
                            // The chosen first, then the rest as preferred.
                            val order = listOf(shown[chosen]) + shown.filterIndexed { i, _ -> i != chosen }
                            engine.moveNotes(order).onSuccess { moved() }.onFailure { error = explain(it) }
                        }
                        busy = false
                    }
                },
            ) { Text(if (shown == null) "See offers" else "Move") }
        },
        dismissButton = { TextButton(onClick = dismiss, enabled = !busy) { Text("Cancel") } },
    )
}
