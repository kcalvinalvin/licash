package org.licash.wallet.ui

import android.content.Context
import android.text.format.Formatter
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.List
import androidx.compose.material.icons.filled.Build
import androidx.compose.material.icons.filled.Info
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material.icons.filled.Place
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.Share
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.licash.core.Hub
import org.licash.wallet.AppLog
import org.licash.wallet.BuildConfig
import org.licash.wallet.Engine
import org.licash.wallet.Storage
import org.licash.wallet.Ui
import org.licash.wallet.explain
import org.licash.wallet.exportLogs
import org.licash.wallet.host
import org.licash.wallet.percent
import org.licash.wallet.storage

/** A page of Settings, of one kind of thing. */
private enum class Page(val title: String) {
    Hubs("Hubs"),
    Bitcoin("Bitcoin"),
    Privacy("Privacy"),
    Storage("Storage"),
    Advanced("Advanced"),
}

/**
 * Settings: a short list of what there is to see and change, each kind on
 * a page of its own.
 */
@Composable
fun Settings(engine: Engine, ui: Ui, balance: () -> Unit, back: () -> Unit) {
    var page by rememberSaveable { mutableStateOf<Page?>(null) }
    BackHandler(enabled = page != null) { page = null }
    Column(
        Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(horizontal = 20.dp),
    ) {
        when (val p = page) {
            null -> {
                TopBar("Settings", back)
                Overview(engine, ui) { page = it }
            }
            else -> {
                TopBar(p.title, back = { page = null })
                when (p) {
                    Page.Hubs -> {
                        var shown by remember { mutableStateOf<Hub?>(null) }
                        Group("") {
                            ui.hubs.forEach { hub ->
                                Entry(name(hub.url), Modifier.testTag("hub"), sub = about(hub), onClick = { shown = hub })
                                Divider()
                            }
                            AddHub(engine)
                        }
                        Group("More hubs") { FoundHubs(engine, ui, auto = false) }
                        shown?.let { HubFees(it, full = true) { shown = null } }
                    }
                    Page.Bitcoin -> {
                        Group("") { Status(ui) }
                        if (ui.developer) Group("Light client") { LightClient(engine, ui) }
                    }
                    Page.Privacy -> Group("") { Privacy(engine, ui) }
                    Page.Storage -> Group("") { StorageUsed(ui.developer) }
                    Page.Advanced -> Group("") { Advanced(engine, balance) }
                }
            }
        }
        Spacer(Modifier.height(48.dp))
    }
}

/** The pages there are, each with where it stands in a word. */
@Composable
private fun Overview(engine: Engine, ui: Ui, open: (Page) -> Unit) {
    val context = LocalContext.current
    var exporting by remember { mutableStateOf<String?>(null) }
    val tint = MaterialTheme.colorScheme.primary
    Group("") {
        Entry("Hubs", value = "${ui.hubs.size}", icon = Icons.Filled.Place, tint = tint, tag = "hubs") { open(Page.Hubs) }
        Divider()
        Entry("Bitcoin", value = status(ui, false).first, icon = Icons.Filled.Refresh, tint = tint, tag = "bitcoin") {
            open(Page.Bitcoin)
        }
        Divider()
        Entry("Privacy", value = if (ui.tor) "Tor on" else "Tor off", icon = Icons.Filled.Lock, tint = tint, tag = "tor") {
            open(Page.Privacy)
        }
    }
    Group("") {
        Entry("Storage", icon = Icons.AutoMirrored.Filled.List, tint = tint, tag = "storage") { open(Page.Storage) }
        Divider()
        Entry("Export logs", icon = Icons.Filled.Share, tint = tint, tag = "export-logs") {
            exporting = null
            runCatching { context.startActivity(exportLogs(context, ui)) }
                .onFailure {
                    AppLog.warn("exporting the logs", it)
                    exporting = "Could not export the logs: ${it.message ?: it.javaClass.simpleName}"
                }
        }
        exporting?.let { Problem(it) }
        Divider()
        Toggle(
            "Advanced mode",
            ui.developer,
            "Shows how payments are made, the notes held, providers to choose from, " +
                "the Bitcoin light client's settings and the log.",
        ) { engine.setDeveloper(it) }
        if (ui.developer) {
            Divider()
            Entry("Advanced", icon = Icons.Filled.Build, tint = tint, tag = "advanced") { open(Page.Advanced) }
        }
    }
    Group("") { Line("Version", BuildConfig.VERSION_NAME, tag = "version") }
}

@Composable
private fun Privacy(engine: Engine, ui: Ui) {
    val scope = rememberCoroutineScope()
    var error by remember { mutableStateOf<String?>(null) }
    Toggle(
        "Tor",
        ui.tor,
        "Reaches hubs, relays and Bitcoin through Tor, each apart: slower, and hides where you are from them.",
    ) { on ->
        scope.launch { engine.setTor(on).onFailure { error = explain(it) } }
    }
    error?.let { Problem(it) }
    if (ui.developer && ui.wallet) {
        Divider()
        RenewLimit(engine)
    }
}

/**
 * The most the wallet pays, on its own, to renew notes a hub that
 * restarted can no longer unlink: in parts per million of them.
 */
@Composable
private fun RenewLimit(engine: Engine) {
    val scope = rememberCoroutineScope()
    var ppm by remember { mutableStateOf(engine.renewLimit()?.toString() ?: "") }
    var error by remember { mutableStateOf<String?>(null) }
    val asked = ppm.trim().toUIntOrNull()
    Column(Modifier.padding(vertical = 12.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Text("Renewing notes", style = MaterialTheme.typography.bodyLarge)
        Text(
            "A hub that restarted can no longer unlink the notes it issued before. " +
                "The wallet swaps them for new ones there if the hub charges at most this.",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            OutlinedTextField(
                value = ppm,
                onValueChange = { ppm = it; error = null },
                modifier = Modifier.weight(1f),
                singleLine = true,
                label = { Text("Most to pay, parts per million") },
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number, imeAction = ImeAction.Done),
                shape = RoundedCornerShape(14.dp),
            )
            FilledTonalButton(
                enabled = asked != null,
                onClick = {
                    if (asked != null) {
                        scope.launch { engine.setRenewLimit(asked).onFailure { error = explain(it) } }
                    }
                },
            ) { Text("Save") }
        }
        Problem(
            error ?: asked?.let { "${percent(it.toULong())} of the notes renewed, 0 for free renewals only." }
                ?: "A whole number, such as 100 (0.01%).",
            error = error != null || asked == null,
        )
    }
}

/** What the app keeps on the phone, by what keeps it; in advanced mode, each network's light client. */
@Composable
private fun StorageUsed(advanced: Boolean) {
    val context = LocalContext.current
    var kept by remember { mutableStateOf<Storage?>(null) }
    LaunchedEffect(Unit) { kept = withContext(Dispatchers.IO) { storage(context) } }
    val size = { b: Long -> bytes(context, b) }
    kept?.let { k ->
        Line("Total", size(k.total), tag = "storage-total")
        Divider()
        Line("Wallet", size(k.wallet))
        Line("Bitcoin light client", size(k.chain), tag = "storage-chain")
        if (advanced) k.chains.forEach { (name, b) -> Line("  $name", size(b)) }
        if (k.tor > 0) Line("Tor", size(k.tor))
        Line("Logs", size(k.logs))
        Line("Cache", size(k.cache))
        if (k.other > 0) Line("Other", size(k.other))
        k.app?.let { Line("The app itself", size(it), tag = "storage-app") }
    }
}

private fun bytes(context: Context, bytes: Long): String = Formatter.formatShortFileSize(context, bytes)

/** Something on or off: what it does, behind an (i). */
@Composable
private fun Toggle(title: String, on: Boolean, means: String? = null, set: (Boolean) -> Unit) {
    Row(
        Modifier.fillMaxWidth().clickable { set(!on) }.heightIn(min = 56.dp).padding(vertical = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(title, style = MaterialTheme.typography.bodyLarge)
        means?.let { Info(title, it) }
        Spacer(Modifier.weight(1f))
        Switch(checked = on, onCheckedChange = set, modifier = Modifier.testTag("toggle $title"))
    }
}

/**
 * How far the app follows Bitcoin, in a word, and what there is to add:
 * in advanced mode, the block it is at and the nodes it hears from.
 */
private fun status(ui: Ui, developer: Boolean): Pair<String, String?> {
    val chain = ui.chain
    val nodes = "${chain.peers} " + if (chain.peers == 1L) "node" else "nodes"
    return when {
        chain.peers == 0L -> "Connecting" to null
        !chain.synced -> "Catching up" to "Block ${chain.height}, $nodes".takeIf { developer }
        ui.scanned != null && ui.scanned.toInt() <= chain.height ->
            "Checking" to "Block ${ui.scanned} of ${chain.height}".takeIf { developer }
        else -> "Up to date" to "Block ${chain.height}, $nodes".takeIf { developer }
    }
}

/** How far the app follows Bitcoin, and what keeps it from being up to date. */
@Composable
private fun Status(ui: Ui) {
    val (title, detail) = status(ui, ui.developer)
    Entry("Bitcoin", value = title, sub = detail, tag = "chain")
    ui.problem?.let { Problem(it) }
}

/** The light client's settings and the block explorer, for those testing them. */
@Composable
private fun LightClient(engine: Engine, ui: Ui) {
    val scope = rememberCoroutineScope()
    var peers by remember { mutableStateOf(engine.prefs.peers) }
    var cdn by remember { mutableStateOf(engine.prefs.cdn) }
    var explorer by remember { mutableStateOf(engine.prefs.explorer) }
    run {
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
        Row(
            Modifier.fillMaxWidth().padding(vertical = 12.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            OutlinedTextField(
                value = explorer,
                onValueChange = { explorer = it },
                modifier = Modifier.weight(1f),
                singleLine = true,
                label = { Text("Block explorer (default, a URL, or empty)") },
                shape = RoundedCornerShape(14.dp),
            )
            FilledTonalButton(onClick = { engine.prefs.explorer = explorer }) { Text("Save") }
        }
    }
}

/** The notes held, the log, and deleting the wallet, for those testing them. */
@Composable
private fun Advanced(engine: Engine, balance: () -> Unit) {
    val scope = rememberCoroutineScope()
    var logs by remember { mutableStateOf(false) }
    var deleting by remember { mutableStateOf(false) }
    run {
        Entry("Balance and notes", onClick = balance)
        Divider()
        Toggle("Show the log", logs) { logs = it }
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
