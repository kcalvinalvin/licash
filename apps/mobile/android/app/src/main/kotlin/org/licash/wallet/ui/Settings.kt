package org.licash.wallet.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.launch
import org.licash.core.Hub
import org.licash.wallet.Engine
import org.licash.wallet.Lines
import org.licash.wallet.Ui
import org.licash.wallet.explain
import org.licash.wallet.sat

@Composable
fun Settings(engine: Engine, ui: Ui, back: () -> Unit) {
    val scope = rememberCoroutineScope()
    var url by remember { mutableStateOf("") }
    var error by remember { mutableStateOf<String?>(null) }
    var taps by remember { mutableIntStateOf(0) }
    Column(Modifier.fillMaxSize().padding(horizontal = 24.dp).verticalScroll(rememberScrollState())) {
        TopBar("Settings", back)
        Section("Hubs")
        for (hub in ui.hubs) HubRow(hub)
        OutlinedTextField(
            value = url,
            onValueChange = { url = it },
            modifier = Modifier.fillMaxWidth(),
            singleLine = true,
            label = { Text("Add a hub") },
            placeholder = { Text("https://hub.example") },
        )
        error?.let { Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall) }
        TextButton(enabled = url.isNotBlank(), onClick = {
            scope.launch {
                engine.addHub(url.trim())
                    .onSuccess { url = ""; error = null }
                    .onFailure { error = explain(it) }
            }
        }) { Text("Add") }

        Section("Privacy")
        Toggle("Reach hubs through Tor", "Slower, and hides where you are from hubs.", ui.tor) { on ->
            scope.launch { engine.setTor(on).onFailure { error = explain(it) } }
        }

        Section("About")
        Text(
            "Licash ${org.licash.wallet.BuildConfig.VERSION_NAME}",
            modifier = Modifier.fillMaxWidth().clickable {
                taps += 1
                if (taps >= 7 && !ui.developer) engine.setDeveloper(true)
            }.padding(vertical = 12.dp),
        )
        if (ui.developer) Developer(engine, ui)
        Spacer(Modifier.height(48.dp))
    }
}

@Composable
private fun Section(title: String) {
    Spacer(Modifier.height(24.dp))
    Text(title, style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.primary)
    Spacer(Modifier.height(8.dp))
}

@Composable
private fun HubRow(hub: Hub) {
    val can = buildList {
        if (hub.deposits != null) add("takes Bitcoin from ${sat(hub.deposits!!.min)}")
        if (hub.silent) add("finds Bitcoin payments")
        if (hub.mailbox) add("mailbox")
    }
    Column(Modifier.padding(vertical = 8.dp)) {
        Text(hub.url, style = MaterialTheme.typography.bodyLarge)
        Text(
            (listOf(fee(hub)) + can).joinToString(" · "),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

private fun fee(hub: Hub): String = when {
    hub.feeBase == 0uL && hub.feePpm == 0uL -> "No fee"
    hub.feePpm == 0uL -> "Fee ${sat(hub.feeBase)}"
    else -> "Fee ${sat(hub.feeBase)} + ${hub.feePpm} ppm"
}

@Composable
private fun Toggle(title: String, detail: String, on: Boolean, set: (Boolean) -> Unit) {
    Row(Modifier.fillMaxWidth().padding(vertical = 8.dp), verticalAlignment = Alignment.CenterVertically) {
        Column(Modifier.weight(1f)) {
            Text(title, style = MaterialTheme.typography.bodyLarge)
            Text(detail, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        Switch(checked = on, onCheckedChange = set)
    }
}

/** The light client and the wallet's log, for those testing them. */
@Composable
private fun Developer(engine: Engine, ui: Ui) {
    val scope = rememberCoroutineScope()
    var peers by remember { mutableStateOf(engine.prefs.peers) }
    var cdn by remember { mutableStateOf(engine.prefs.cdn) }
    var logs by remember { mutableStateOf(false) }
    var deleting by remember { mutableStateOf(false) }
    Section("Developer")
    Toggle("Developer mode", "Test networks and the light client's settings.", true) { engine.setDeveloper(false) }
    val chain = ui.chain
    Text(
        "${ui.network.name} · block ${chain.height} · ${chain.peers} " +
            (if (chain.peers == 1L) "peer · " else "peers · ") +
            (if (chain.synced) "synced" else "syncing") + (ui.scanned?.let { " · scanned to $it" } ?: ""),
        style = MaterialTheme.typography.bodySmall,
    )
    ui.problem?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error) }
    OutlinedTextField(
        value = peers,
        onValueChange = { peers = it },
        modifier = Modifier.fillMaxWidth(),
        singleLine = true,
        label = { Text("Peers (host:port, comma separated)") },
    )
    OutlinedTextField(
        value = cdn,
        onValueChange = { cdn = it },
        modifier = Modifier.fillMaxWidth(),
        singleLine = true,
        label = { Text("block-dn (default, a URL, or empty)") },
    )
    TextButton(onClick = { scope.launch { engine.setChain(peers, cdn) } }) { Text("Apply") }
    TextButton(onClick = { logs = !logs }) { Text(if (logs) "Hide log" else "Show log") }
    if (logs) {
        Text(
            Lines.recent().takeLast(80).joinToString("\n"),
            fontFamily = FontFamily.Monospace,
            fontSize = 10.sp,
            lineHeight = 12.sp,
        )
    }
    OutlinedButton(onClick = { deleting = true }, modifier = Modifier.fillMaxWidth()) {
        Text("Delete wallet", color = MaterialTheme.colorScheme.error)
    }
    if (deleting) {
        AlertDialog(
            onDismissRequest = { deleting = false },
            title = { Text("Delete this wallet?") },
            text = { Text("What it holds is lost for good: there is no backup yet.") },
            confirmButton = {
                TextButton(onClick = {
                    deleting = false
                    scope.launch { engine.delete() }
                }) { Text("Delete", color = MaterialTheme.colorScheme.error) }
            },
            dismissButton = { TextButton(onClick = { deleting = false }) { Text("Cancel") } },
        )
    }
}
