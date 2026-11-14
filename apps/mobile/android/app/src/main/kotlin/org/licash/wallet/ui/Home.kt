package org.licash.wallet.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import org.licash.core.Activity
import org.licash.core.Kind
import org.licash.core.State
import org.licash.wallet.Engine
import org.licash.wallet.Ui
import org.licash.wallet.at
import org.licash.wallet.digits
import org.licash.wallet.explain
import org.licash.wallet.sat

@Composable
fun Home(engine: Engine, ui: Ui, receive: () -> Unit, send: () -> Unit, settings: () -> Unit) {
    Column(Modifier.fillMaxSize().padding(horizontal = 24.dp)) {
        Row(Modifier.fillMaxWidth().padding(top = 8.dp), verticalAlignment = Alignment.CenterVertically) {
            Wordmark()
            Spacer(Modifier.width(10.dp))
            NetworkChip(ui.network)
            Spacer(Modifier.weight(1f))
            IconButton(onClick = settings) { Icon(Icons.Default.Settings, contentDescription = "Settings") }
        }
        Spacer(Modifier.height(40.dp))
        Total(ui)
        Spacer(Modifier.height(32.dp))
        if (ui.hubs.isEmpty()) {
            ConnectHub(engine)
        } else {
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                BigButton("Receive", Modifier.weight(1f), onClick = receive)
                BigButton("Send", Modifier.weight(1f), onClick = send)
            }
        }
        Spacer(Modifier.height(28.dp))
        if (ui.history.isEmpty()) {
            Text(
                "Nothing yet. What you receive and send shows here.",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        } else {
            LazyColumn(Modifier.fillMaxSize()) {
                items(ui.history, key = { it.id + it.kind }) { activity ->
                    ActivityRow(activity)
                    HorizontalDivider(color = MaterialTheme.colorScheme.outline.copy(alpha = 0.4f))
                }
            }
        }
    }
}

@Composable
private fun Total(ui: Ui) {
    val b = ui.balance
    val total = b.notes + b.notesPending + b.onchain + b.moving
    Row(verticalAlignment = Alignment.Bottom) {
        Text(digits(total), style = Figures)
        Text(
            " sat",
            modifier = Modifier.padding(bottom = 8.dp),
            style = MaterialTheme.typography.titleMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
    val notes = buildList {
        if (b.moving > 0uL) add("${sat(b.moving)} on its way from Bitcoin")
        if (b.onchain > 0uL) add("${sat(b.onchain)} on Bitcoin, moved in once enough")
        if (b.notesPending > 0uL) add("${sat(b.notesPending)} still settling")
        if (!ui.chain.synced && ui.hubs.isNotEmpty()) add("Catching up with Bitcoin…")
        ui.problem?.let { add(it) }
    }
    for (note in notes) {
        Text(
            note,
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            maxLines = 2,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

/** No hub yet: nothing works without one. */
@Composable
private fun ConnectHub(engine: Engine) {
    val scope = rememberCoroutineScope()
    var url by rememberSaveable { mutableStateOf("") }
    var busy by rememberSaveable { mutableStateOf(false) }
    var error by rememberSaveable { mutableStateOf<String?>(null) }
    Surface(shape = RoundedCornerShape(20.dp), color = MaterialTheme.colorScheme.surfaceContainer) {
        Column(Modifier.padding(20.dp)) {
            Text("Connect to a hub", style = MaterialTheme.typography.titleMedium)
            Text(
                "A hub holds your Licash and moves it for you. Paste the address its operator gave you.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.height(12.dp))
            OutlinedTextField(
                value = url,
                onValueChange = { url = it },
                modifier = Modifier.fillMaxWidth(),
                singleLine = true,
                placeholder = { Text("https://hub.example") },
            )
            error?.let { Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall) }
            Spacer(Modifier.height(12.dp))
            BigButton(if (busy) "Connecting…" else "Connect", Modifier.fillMaxWidth(), enabled = !busy && url.isNotBlank()) {
                busy = true
                error = null
                scope.launch {
                    engine.addHub(url.trim()).onFailure { error = explain(it) }
                    busy = false
                }
            }
        }
    }
}

@Composable
private fun ActivityRow(activity: Activity) {
    val (title, sign) = when (activity.kind) {
        Kind.SENT -> "Sent" to "−"
        Kind.RECEIVED -> "Received" to "+"
        Kind.MOVED -> "From Bitcoin" to "+"
        Kind.ONCHAIN -> "On Bitcoin" to "+"
    }
    val detail = when (activity.state) {
        State.PENDING -> "Under way"
        State.FAILED -> activity.detail
        State.DONE -> activity.at?.let { at(it) } ?: activity.detail
    }
    val color = when (activity.state) {
        State.FAILED -> MaterialTheme.colorScheme.error
        else -> MaterialTheme.colorScheme.onSurface
    }
    Row(Modifier.fillMaxWidth().padding(vertical = 14.dp), verticalAlignment = Alignment.CenterVertically) {
        Column(Modifier.weight(1f)) {
            Text(title, style = MaterialTheme.typography.bodyLarge)
            Text(
                detail,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
        Text("$sign${digits(activity.amount)}", style = MaterialTheme.typography.bodyLarge, color = color)
    }
}
