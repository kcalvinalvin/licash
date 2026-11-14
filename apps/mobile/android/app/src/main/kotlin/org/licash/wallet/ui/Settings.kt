package org.licash.wallet.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.filled.Share
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
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
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.launch
import org.licash.core.Hub
import org.licash.wallet.AppLog
import org.licash.wallet.BuildConfig
import org.licash.wallet.Engine
import org.licash.wallet.Ui
import org.licash.wallet.explain
import org.licash.wallet.exportLogs
import org.licash.wallet.sat

@Composable
fun Settings(engine: Engine, ui: Ui, back: () -> Unit) {
    val scope = rememberCoroutineScope()
    val context = LocalContext.current
    var taps by remember { mutableIntStateOf(0) }
    var error by remember { mutableStateOf<String?>(null) }
    Column(
        Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(horizontal = 20.dp),
    ) {
        TopBar("Settings", back)
        Group("Hubs") {
            ui.hubs.forEachIndexed { i, hub ->
                if (i > 0) Divider()
                HubRow(hub)
            }
            if (ui.hubs.isNotEmpty()) Divider()
            AddHub(engine)
        }
        Group("Privacy") {
            Toggle("Reach hubs through Tor", "Slower, and hides where you are from hubs.", ui.tor) { on ->
                scope.launch { engine.setTor(on).onFailure { error = explain(it) } }
            }
            error?.let { Note(it, error = true) }
        }
        Group("Bitcoin") {
            Status(ui)
        }
        Group("Support") {
            Action("Export logs", "Share what the app did, to report a problem.", Icons.Filled.Share) {
                runCatching { context.startActivity(exportLogs(context, ui)) }
                    .onFailure { AppLog.warn("exporting the logs", it) }
            }
        }
        Group("About") {
            Row(
                Modifier.fillMaxWidth().clickable {
                    taps += 1
                    if (taps >= 7 && !ui.developer) engine.setDeveloper(true)
                }.padding(vertical = 14.dp),
            ) {
                Text("Version", Modifier.weight(1f), style = MaterialTheme.typography.bodyLarge)
                Text(
                    BuildConfig.VERSION_NAME,
                    style = MaterialTheme.typography.bodyLarge,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
        if (ui.developer) Developer(engine, ui)
        Spacer(Modifier.height(48.dp))
    }
}

/** A titled card of rows. */
@Composable
private fun Group(title: String, content: @Composable ColumnScope.() -> Unit) {
    Spacer(Modifier.height(20.dp))
    Text(
        title,
        modifier = Modifier.padding(start = 4.dp, bottom = 8.dp),
        style = MaterialTheme.typography.labelLarge,
        color = MaterialTheme.colorScheme.primary,
    )
    Surface(
        modifier = Modifier.fillMaxWidth(),
        shape = RoundedCornerShape(20.dp),
        color = MaterialTheme.colorScheme.surfaceContainer,
    ) {
        Column(Modifier.padding(horizontal = 16.dp, vertical = 4.dp), content = content)
    }
}

@Composable
private fun Divider() = HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)

@Composable
private fun Note(text: String, error: Boolean = false) {
    Text(
        text,
        modifier = Modifier.padding(bottom = 12.dp),
        style = MaterialTheme.typography.bodySmall,
        color = if (error) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurfaceVariant,
    )
}

@Composable
private fun HubRow(hub: Hub) {
    val can = buildList {
        add(fee(hub))
        hub.deposits?.let { add("takes Bitcoin from ${sat(it.min)}") }
        if (hub.silent) add("finds Bitcoin payments")
        if (hub.mailbox) add("mailbox")
    }
    Column(Modifier.fillMaxWidth().padding(vertical = 12.dp)) {
        Text(hub.url, style = MaterialTheme.typography.bodyLarge, maxLines = 1)
        Spacer(Modifier.height(2.dp))
        Text(
            can.joinToString(" · "),
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
private fun AddHub(engine: Engine) {
    val scope = rememberCoroutineScope()
    var url by remember { mutableStateOf("") }
    var adding by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    fun add() {
        adding = true
        scope.launch {
            engine.addHub(url.trim())
                .onSuccess { url = ""; error = null }
                .onFailure { error = explain(it) }
            adding = false
        }
    }
    Row(
        Modifier.fillMaxWidth().padding(vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        OutlinedTextField(
            value = url,
            onValueChange = { url = it },
            modifier = Modifier.weight(1f),
            singleLine = true,
            placeholder = { Text("Add a hub's address") },
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri, imeAction = ImeAction.Done),
            shape = RoundedCornerShape(14.dp),
        )
        FilledTonalButton(enabled = url.isNotBlank() && !adding, onClick = ::add) { Text("Add") }
    }
    error?.let { Note(it, error = true) }
}

@Composable
private fun Toggle(title: String, detail: String, on: Boolean, set: (Boolean) -> Unit) {
    Row(
        Modifier.fillMaxWidth().clickable { set(!on) }.padding(vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(title, style = MaterialTheme.typography.bodyLarge)
            Text(detail, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        Spacer(Modifier.width(12.dp))
        Switch(checked = on, onCheckedChange = set)
    }
}

@Composable
private fun Action(title: String, detail: String, icon: ImageVector, onClick: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().clickable(onClick = onClick).padding(vertical = 14.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(icon, contentDescription = null, tint = MaterialTheme.colorScheme.primary)
        Spacer(Modifier.width(16.dp))
        Column(Modifier.weight(1f)) {
            Text(title, style = MaterialTheme.typography.bodyLarge)
            Text(detail, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        Icon(
            Icons.AutoMirrored.Filled.KeyboardArrowRight,
            contentDescription = null,
            tint = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

/** How far the app follows Bitcoin, in words. */
@Composable
private fun Status(ui: Ui) {
    val chain = ui.chain
    val (title, detail) = when {
        chain.peers == 0L -> "Connecting" to "Looking for Bitcoin nodes."
        !chain.synced -> "Catching up" to "At block ${chain.height}."
        ui.scanned != null && ui.scanned.toInt() <= chain.height ->
            "Looking for payments" to "At block ${ui.scanned} of ${chain.height}."
        else -> "Up to date" to "Block ${chain.height}, ${chain.peers} " + (if (chain.peers == 1L) "node." else "nodes.")
    }
    Column(Modifier.fillMaxWidth().padding(vertical = 12.dp)) {
        Text(title, style = MaterialTheme.typography.bodyLarge)
        Text(detail, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }
    ui.problem?.let { Note(it, error = true) }
}

/** The light client's settings and the log, for those testing them. */
@Composable
private fun Developer(engine: Engine, ui: Ui) {
    val scope = rememberCoroutineScope()
    var peers by remember { mutableStateOf(engine.prefs.peers) }
    var cdn by remember { mutableStateOf(engine.prefs.cdn) }
    var logs by remember { mutableStateOf(false) }
    var deleting by remember { mutableStateOf(false) }
    Group("Developer") {
        Toggle("Developer mode", "Test networks and the light client's settings.", true) {
            engine.setDeveloper(false)
        }
        Divider()
        Column(Modifier.padding(vertical = 12.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Text("${ui.network.name} light client", style = MaterialTheme.typography.bodyLarge)
            OutlinedTextField(
                value = peers,
                onValueChange = { peers = it },
                modifier = Modifier.fillMaxWidth(),
                singleLine = true,
                label = { Text("Peers (host:port, comma separated)") },
                shape = RoundedCornerShape(14.dp),
            )
            OutlinedTextField(
                value = cdn,
                onValueChange = { cdn = it },
                modifier = Modifier.fillMaxWidth(),
                singleLine = true,
                label = { Text("block-dn (default, a URL, or empty)") },
                shape = RoundedCornerShape(14.dp),
            )
            FilledTonalButton(onClick = { scope.launch { engine.setChain(peers, cdn) } }) { Text("Apply") }
        }
        Divider()
        Toggle("Show the log", "Its newest lines, here.", logs) { logs = it }
        if (logs) {
            Text(
                AppLog.recent().takeLast(80).joinToString("\n"),
                modifier = Modifier.padding(bottom = 12.dp),
                fontFamily = FontFamily.Monospace,
                fontSize = 10.sp,
                lineHeight = 12.sp,
            )
        }
        Divider()
        TextButton(onClick = { deleting = true }, modifier = Modifier.padding(vertical = 4.dp)) {
            Text("Delete wallet", color = MaterialTheme.colorScheme.error)
        }
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
