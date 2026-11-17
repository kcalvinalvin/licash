package org.licash.wallet.ui

import android.content.Context
import android.text.format.Formatter
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Build
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material.icons.outlined.Info
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
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
import org.licash.wallet.percent
import org.licash.wallet.storage

/** A page of Settings, behind a row of its first. */
private enum class Page(val title: String) {
    Hubs("Hubs"),
    Backup("Backup"),
    About("About"),
    Advanced("Advanced"),
}

private const val TOR_MEANS =
    "Reaches hubs, relays and Bitcoin through Tor, each apart: slower, and hides where you are from them."

private const val ADVANCED_MEANS =
    "Shows how payments are made, the notes held, providers to choose from, " +
        "the Bitcoin light client's settings and the log."

/**
 * Settings: on its first page, a row for each kind of thing, each with a
 * glyph on a tile. What is on or off switches there; what holds more
 * opens a page of its own.
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
                    Page.Hubs -> HubsPage(engine, ui)
                    Page.Backup -> BackupPage(engine, ui)
                    Page.About -> About(ui)
                    Page.Advanced -> Advanced(engine, ui, balance)
                }
            }
        }
        Spacer(Modifier.height(48.dp))
    }
}

/** The first page: hubs; Bitcoin and Tor; about the app; advanced mode. */
@Composable
private fun Overview(engine: Engine, ui: Ui, open: (Page) -> Unit) {
    val scope = rememberCoroutineScope()
    var torFailed by remember { mutableStateOf<String?>(null) }
    Group("") {
        Entry("Hubs", value = "${ui.hubs.size}", icon = Glyphs.bank, tag = "hubs") { open(Page.Hubs) }
        if (ui.wallet) {
            Divider()
            Entry("Backup", icon = Glyphs.key, tag = "backup") { open(Page.Backup) }
        }
    }
    Group("") {
        val (state, detail) = status(ui, ui.developer)
        Entry("Bitcoin", value = state, sub = detail, icon = Glyphs.bitcoin, tag = "chain")
        ui.problem?.let { Problem(it) }
        Divider()
        Toggle("Tor", ui.tor, sub = "Hides where you are, slower", icon = Icons.Filled.Lock, means = TOR_MEANS) { on ->
            torFailed = null
            scope.launch { engine.setTor(on).onFailure { torFailed = explain(it) } }
        }
        torFailed?.let { Problem(it) }
    }
    Group("") {
        Entry("About", icon = Icons.Outlined.Info, tag = "about") { open(Page.About) }
    }
    Group("") {
        Toggle("Advanced mode", ui.developer, icon = Glyphs.code, means = ADVANCED_MEANS) { engine.setDeveloper(it) }
        if (ui.developer) {
            Divider()
            Entry("Advanced", icon = Icons.Filled.Build, tag = "advanced") { open(Page.Advanced) }
        }
    }
}

/** The hubs used, each opening what it charges; one added by its address, or found. */
@Composable
private fun HubsPage(engine: Engine, ui: Ui) {
    val scope = rememberCoroutineScope()
    var shown by remember { mutableStateOf<Hub?>(null) }
    var optimizing by remember { mutableStateOf<Hub?>(null) }
    var failed by remember { mutableStateOf<String?>(null) }
    fun raised(hub: Hub) = ui.raised.find { it.hub == hub.url }
    if (ui.hubs.isNotEmpty()) {
        Group("") {
            ui.hubs.forEachIndexed { i, hub ->
                if (i > 0) Divider()
                val sub = if (raised(hub) == null) about(hub) else "Raised its fees: approve to pay there"
                Entry(name(hub.url), Modifier.testTag("hub"), sub = sub, onClick = { shown = hub })
            }
        }
    }
    failed?.let { Problem(it) }
    Group("Add a hub") { AddHub(engine) }
    Group("More hubs") { FoundHubs(engine, ui, auto = false) }
    shown?.let { hub ->
        HubFees(
            hub,
            full = true,
            raised = raised(hub),
            approve = { r ->
                shown = null
                scope.launch { failed = engine.approveFees(r).exceptionOrNull()?.let(::explain) }
            },
            optimize = if (ui.developer) ({ shown = null; optimizing = hub }) else null,
        ) { shown = null }
    }
    optimizing?.let { hub -> OptimizeNotes(engine, hub) { optimizing = null } }
}

/** The app's version, what it keeps on the phone, and its logs to share. */
@Composable
private fun About(ui: Ui) {
    val context = LocalContext.current
    var kept by remember { mutableStateOf<Storage?>(null) }
    var sizes by remember { mutableStateOf(false) }
    var exporting by remember { mutableStateOf<String?>(null) }
    LaunchedEffect(Unit) { kept = withContext(Dispatchers.IO) { storage(context) } }
    Group("") {
        Line("Version", BuildConfig.VERSION_NAME, tag = "version")
        Divider()
        Entry("Storage", value = kept?.let { bytes(context, it.total) } ?: "…", tag = "storage") {
            // Opened once measured, if tapped before.
            sizes = true
        }
        Divider()
        Entry("Export logs", tag = "export-logs") {
            exporting = null
            runCatching { context.startActivity(exportLogs(context, ui)) }
                .onFailure {
                    AppLog.warn("exporting the logs", it)
                    exporting = "Could not export the logs: ${it.message ?: it.javaClass.simpleName}"
                }
        }
        exporting?.let { Problem(it) }
    }
    val k = kept
    if (sizes && k != null) Sheet("Storage", dismiss = { sizes = false }) { StorageUsed(k, ui.developer) }
}

/** What the app keeps on the phone, by what keeps it; in advanced mode, each network's light client. */
@Composable
private fun StorageUsed(k: Storage, advanced: Boolean) {
    val context = LocalContext.current
    val size = { b: Long -> bytes(context, b) }
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

private fun bytes(context: Context, bytes: Long): String = Formatter.formatShortFileSize(context, bytes)

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

/**
 * For those testing the app: the balance's notes, the log, what renewing
 * notes may cost, the light client's settings, and deleting the wallet.
 */
@Composable
private fun Advanced(engine: Engine, ui: Ui, balance: () -> Unit) {
    val scope = rememberCoroutineScope()
    var logs by remember { mutableStateOf(false) }
    var deleting by remember { mutableStateOf(false) }
    Group("") {
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
    }
    if (ui.wallet) Group("Renewing notes") { RenewLimit(engine) }
    if (ui.wallet) Group("Recovery safety") { ProofMargin(engine) }
    Group("Bitcoin light client") {
        LightClient(engine)
        Divider()
        ResetChain(engine)
    }
    Group("") {
        Entry("Delete wallet", tag = "delete-wallet", color = MaterialTheme.colorScheme.error) { deleting = true }
    }
    if (deleting) {
        Sheet("Delete this wallet?", dismiss = { deleting = false }) {
            Say("What it holds is lost for good, unless you saved a backup file.")
            BigButton("Delete", Modifier.fillMaxWidth().testTag("delete"), danger = true) {
                deleting = false
                scope.launch { engine.delete() }
            }
            Spacer(Modifier.height(12.dp))
            BigButton("Cancel", Modifier.fillMaxWidth(), quiet = true) { deleting = false }
        }
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
    var saved by remember { mutableStateOf(false) }
    val asked = ppm.trim().toUIntOrNull()
    Row(verticalAlignment = Alignment.CenterVertically) {
        Text("Most to pay", style = MaterialTheme.typography.bodyLarge)
        Info(
            "Renewing notes",
            "A hub that restarted can no longer unlink the notes it issued before. " +
                "The wallet swaps them for new ones there within this share, or if its fees are at most " +
                "1 sat base, this rate and 0.1 sat per output. Protection's agreed fees take precedence. 0 means free only.",
        )
    }
    Row(
        Modifier.fillMaxWidth().padding(bottom = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Field(
            ppm,
            { ppm = it; error = null; saved = false },
            "Parts per million",
            error ?: asked?.let { "${percent(it.toULong())} of the notes renewed" } ?: "0 for free renewals only",
            Modifier.weight(1f),
            failed = error != null || (ppm.isNotBlank() && asked == null),
            number = true,
        )
        FilledTonalButton(
            enabled = asked != null && !saved,
            onClick = {
                if (asked != null) {
                    scope.launch {
                        engine.setRenewLimit(asked).onSuccess { saved = true }.onFailure { error = explain(it) }
                    }
                }
            },
        ) { Text(if (saved) "Saved" else "Save") }
    }
}

/** A holder-owned planning margin, never automatic spending consent. */
@Composable
private fun ProofMargin(engine: Engine) {
    var days by remember { mutableStateOf(engine.prefs.proofMarginDays.toString()) }
    var saved by remember { mutableStateOf(false) }
    val value = days.toIntOrNull()?.takeIf { it in 1..30 }
    Say("Time reserved for a disputed proof, about 14 days by default. This is planning only, not automatic closing. Your wallet must stay online and confirm its timeout promptly; a hub may answer late until that timeout wins.")
    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
        Field(days, { days = it; saved = false }, "Days", "1 to 30, estimated from block times", Modifier.weight(1f), failed = value == null, number = true)
        FilledTonalButton(enabled = value != null && !saved, onClick = {
            value?.let { engine.prefs.proofMarginDays = it; saved = true }
        }) { Text(if (saved) "Saved" else "Save") }
    }
}

/** The light client's nodes and block server, and the block explorer that payments link to. */
@Composable
private fun LightClient(engine: Engine) {
    val scope = rememberCoroutineScope()
    var peers by remember { mutableStateOf(engine.prefs.peers) }
    var cdn by remember { mutableStateOf(engine.prefs.cdn) }
    var explorer by remember { mutableStateOf(engine.prefs.explorer) }
    Column(Modifier.padding(vertical = 12.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
        Field(peers, { peers = it }, "Nodes", "host:port, separated by commas")
        Field(cdn, { cdn = it }, "Block server", "default, a URL, or empty for none")
        FilledTonalButton(onClick = { scope.launch { engine.setChain(peers, cdn) } }) { Text("Apply") }
    }
    Divider()
    Row(
        Modifier.fillMaxWidth().padding(vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Field(explorer, { explorer = it }, "Block explorer", "default, a URL, or empty for none", Modifier.weight(1f))
        FilledTonalButton(onClick = { engine.prefs.explorer = explorer }) { Text("Save") }
    }
}

/** A setting typed in: its name in the field, and a hint under it, red if `failed`. */
@Composable
private fun Field(
    value: String,
    change: (String) -> Unit,
    label: String,
    hint: String,
    modifier: Modifier = Modifier.fillMaxWidth(),
    failed: Boolean = false,
    number: Boolean = false,
) {
    OutlinedTextField(
        value = value,
        onValueChange = change,
        modifier = modifier,
        singleLine = true,
        label = { Text(label) },
        supportingText = { Text(hint) },
        isError = failed,
        keyboardOptions = KeyboardOptions(
            keyboardType = if (number) KeyboardType.Number else KeyboardType.Uri,
            imeAction = ImeAction.Done,
        ),
        shape = RoundedCornerShape(14.dp),
    )
}
