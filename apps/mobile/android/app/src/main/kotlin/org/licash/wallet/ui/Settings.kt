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
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.automirrored.filled.List
import androidx.compose.material.icons.filled.Build
import androidx.compose.material.icons.filled.Info
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.KeyboardArrowUp
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material.icons.filled.Place
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.Share
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.Icon
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
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.vector.ImageVector
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
import org.licash.core.FoundHub
import org.licash.core.HubsFound
import org.licash.core.Hub
import org.licash.core.Rate
import org.licash.wallet.AppLog
import org.licash.wallet.BuildConfig
import org.licash.wallet.Engine
import org.licash.wallet.Storage
import org.licash.wallet.Ui
import org.licash.wallet.explain
import org.licash.wallet.exportLogs
import org.licash.wallet.host
import org.licash.wallet.percent
import org.licash.wallet.sat
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
                TopBar(p.title) { page = null }
                when (p) {
                    Page.Hubs -> {
                        Group("Hubs you use") {
                            ui.hubs.forEachIndexed { i, hub ->
                                if (i > 0) Divider()
                                HubRow(hub)
                            }
                            if (ui.hubs.isNotEmpty()) Divider()
                            AddHub(engine)
                        }
                        Group(if (ui.hubs.isEmpty()) "Find a hub" else "Other hubs") { FindHubs(engine, ui) }
                    }
                    Page.Bitcoin -> {
                        Group("Following Bitcoin") { Status(ui) }
                        if (ui.developer) Group("Light client") { LightClient(engine, ui) }
                    }
                    Page.Privacy -> Group("") { Privacy(engine, ui) }
                    Page.Storage -> Group("Kept on this phone") { StorageUsed(ui.developer) }
                    Page.Advanced -> Group("") { Advanced(engine, balance) }
                }
            }
        }
        Spacer(Modifier.height(48.dp))
    }
}

/** The pages there are, each with what it holds in a few words. */
@Composable
private fun Overview(engine: Engine, ui: Ui, open: (Page) -> Unit) {
    val context = LocalContext.current
    var exporting by remember { mutableStateOf<String?>(null) }
    Group("Wallet") {
        val hubs = when (ui.hubs.size) {
            0 -> "None yet: add one"
            1 -> host(ui.hubs[0].url) + ", its fees"
            else -> "${ui.hubs.size} hubs, their fees"
        }
        Action("Hubs", hubs, Icons.Filled.Place) { open(Page.Hubs) }
        Divider()
        Action("Bitcoin", status(ui).first, Icons.Filled.Refresh) { open(Page.Bitcoin) }
        Divider()
        Action("Privacy", if (ui.tor) "Through Tor" else "Tor off", Icons.Filled.Lock) { open(Page.Privacy) }
    }
    Group("App") {
        Action("Storage", "What the app keeps on this phone", Icons.AutoMirrored.Filled.List) { open(Page.Storage) }
        Divider()
        Action("Export logs", "Share what the app did, to report a problem.", Icons.Filled.Share) {
            exporting = null
            runCatching { context.startActivity(exportLogs(context, ui)) }
                .onFailure {
                    AppLog.warn("exporting the logs", it)
                    exporting = "Could not export the logs: ${it.message ?: it.javaClass.simpleName}"
                }
        }
        exporting?.let { Note(it, error = true) }
        Divider()
        Toggle(
            "Advanced mode",
            "Notes by denomination, payment details, providers to choose, the light client's settings and the log.",
            ui.developer,
        ) { engine.setDeveloper(it) }
        if (ui.developer) {
            Divider()
            Action("Advanced", "Notes held, the log, deleting the wallet", Icons.Filled.Build) { open(Page.Advanced) }
        }
    }
    Group("About") {
        Row(Modifier.fillMaxWidth().padding(vertical = 14.dp)) {
            Text("Version", Modifier.weight(1f), style = MaterialTheme.typography.bodyLarge)
            Text(
                BuildConfig.VERSION_NAME,
                style = MaterialTheme.typography.bodyLarge,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

@Composable
private fun Privacy(engine: Engine, ui: Ui) {
    val scope = rememberCoroutineScope()
    var error by remember { mutableStateOf<String?>(null) }
    Toggle("Reach hubs through Tor", "Slower, and hides where you are from hubs.", ui.tor) { on ->
        scope.launch { engine.setTor(on).onFailure { error = explain(it) } }
    }
    error?.let { Note(it, error = true) }
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
        Note(
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
        Size("Total", k.total, "Kept on this phone", size)
        Size("Wallet", k.wallet, "Your notes, payments and keys", size)
        val networks = k.chains.joinToString(", ") { (name, b) -> "$name ${size(b)}" }
        val chain = if (advanced && networks.isNotEmpty()) networks else "Block headers and filters"
        Size("Bitcoin light client", k.chain, chain, size)
        if (k.tor > 0) Size("Tor", k.tor, "What it knows of the Tor network", size)
        Size("Logs", k.logs, "What the app did, to report a problem", size)
        Size("Cache", k.cache, "Made again when needed", size)
        if (k.other > 0) Size("Other", k.other, "Settings and the rest", size)
        k.app?.let { Size("The app itself", it, "Its code and libraries", size) }
    }
}

@Composable
private fun Size(what: String, bytes: Long, how: String, size: (Long) -> String) {
    Row(Modifier.fillMaxWidth().padding(vertical = 8.dp)) {
        Column(Modifier.weight(1f)) {
            Text(what, style = MaterialTheme.typography.bodyLarge)
            Text(how, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        Spacer(Modifier.width(12.dp))
        Text(size(bytes), style = MaterialTheme.typography.bodyLarge, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }
}

private fun bytes(context: Context, bytes: Long): String = Formatter.formatShortFileSize(context, bytes)

@Composable
private fun Note(text: String, error: Boolean = false) {
    Text(
        text,
        modifier = Modifier.padding(bottom = 12.dp),
        style = MaterialTheme.typography.bodySmall,
        color = if (error) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurfaceVariant,
    )
}

/** A hub trusted, and what it does; tapped, what it charges for each. */
@Composable
private fun HubRow(hub: Hub) {
    var open by remember { mutableStateOf(false) }
    val can = buildList {
        add(fee(hub))
        hub.deposits?.let { add("takes Bitcoin from ${sat(it.min)}") }
        if (hub.silent) add("finds Bitcoin payments")
        if (hub.mailbox) add("mailbox")
        if (hub.contracts != null) add("opens contracts")
    }
    Row(Modifier.fillMaxWidth().clickable { open = !open }.padding(vertical = 12.dp)) {
        Column(Modifier.weight(1f)) {
            Text(host(hub.url), style = MaterialTheme.typography.bodyLarge, maxLines = 1, overflow = TextOverflow.Ellipsis)
            Spacer(Modifier.height(2.dp))
            Text(
                can.joinToString(" · "),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            if (open) {
                SelectionContainer {
                    Text(
                        hub.url,
                        modifier = Modifier.padding(top = 8.dp),
                        style = MaterialTheme.typography.bodySmall,
                        fontFamily = FontFamily.Monospace,
                    )
                }
                Fees(hub)
            }
        }
        Icon(
            if (open) Icons.Filled.KeyboardArrowUp else Icons.Filled.KeyboardArrowDown,
            contentDescription = if (open) "Hide fees" else "Show fees",
            tint = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

/** What the hub charges, for each thing it does. */
@Composable
private fun Fees(hub: Hub) {
    Column(Modifier.padding(top = 8.dp)) {
        Text("Fees", style = MaterialTheme.typography.titleSmall)
        FeeLine("Paying in Licash", Rate(hub.feeBase, hub.feePpm), "of each payment")
        hub.lightning?.let {
            FeeLine("Paying over Lightning", it.pay, "on top of the invoice, routing included")
            val range = "${sat(it.receiveRange.min)} to ${sat(it.receiveRange.max)}"
            FeeLine("Receiving over Lightning", it.receive, "on top, paid by the payer; $range")
        }
        hub.depositFee?.let { FeeLine("Moving Bitcoin into Licash", it, "of what is moved") }
        FeeLine("Holding a payment for a condition", hub.lockFee, "as paying an invoice does")
        FeeLine("Refunding a payment held", hub.refundFee, "if it is not paid")
        hub.contracts?.let {
            FeeLine(
                "Opening a contract",
                Rate(it.price, 0uL),
                "from a coin on Bitcoin; protects up to ${sat(it.cap)} for up to ${it.blocks} blocks" +
                    if (it.mock) ", a mock for testing" else "",
            )
        }
    }
}

@Composable
private fun FeeLine(what: String, rate: Rate, how: String) {
    Row(Modifier.fillMaxWidth().padding(vertical = 4.dp)) {
        Column(Modifier.weight(1f)) {
            Text(what, style = MaterialTheme.typography.bodyMedium)
            Text(how, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        Spacer(Modifier.width(12.dp))
        Text(rate(rate), style = MaterialTheme.typography.bodyMedium)
    }
}

private fun rate(rate: Rate): String = when {
    rate.base == 0uL && rate.ppm == 0uL -> "None"
    rate.ppm == 0uL -> sat(rate.base)
    rate.base == 0uL -> percent(rate.ppm)
    else -> "${sat(rate.base)} + ${percent(rate.ppm)}"
}

private fun fee(hub: Hub): String = when {
    hub.feeBase == 0uL && hub.feePpm == 0uL -> "No fee"
    hub.feePpm == 0uL -> "Fee ${sat(hub.feeBase)}"
    hub.feeBase == 0uL -> "Fee ${percent(hub.feePpm)}"
    else -> "Fee ${sat(hub.feeBase)} + ${percent(hub.feePpm)}"
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

/**
 * The hubs announced on Nostr, on the relays the hubs used name and on the
 * default ones: each added with a tap, once the owner says she trusts it.
 * Those announced but not answering are only counted, with a hint if Tor
 * would reach them.
 */
@Composable
private fun FindHubs(engine: Engine, ui: Ui) {
    val scope = rememberCoroutineScope()
    var found by remember { mutableStateOf<HubsFound?>(null) }
    var looking by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    var adding by remember { mutableStateOf<FoundHub?>(null) }
    fun look() {
        looking = true
        error = null
        scope.launch {
            engine.findHubs()
                .onSuccess { found = it }
                .onFailure { error = explain(it) }
            looking = false
        }
    }
    // Those added since looking drop out.
    val shown = found?.hubs?.filter { f -> ui.hubs.none { it.url == f.hub.url } }
    Note(
        if (ui.tor) "Hubs announce themselves on Nostr relays, reached through Tor. Find them there, and add one without typing its address."
        else "Hubs announce themselves on Nostr relays. Find them there, and add one without typing its address.",
    )
    shown?.let { list ->
        if (list.isEmpty()) Note("No other hub answered for now.")
        list.forEachIndexed { i, f ->
            if (i > 0) Divider()
            Row(Modifier.fillMaxWidth().padding(vertical = 12.dp), verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f)) {
                    Text(f.name ?: host(f.hub.url), style = MaterialTheme.typography.bodyLarge, maxLines = 1, overflow = TextOverflow.Ellipsis)
                    Text(
                        listOfNotNull(
                            host(f.hub.url),
                            fee(f.hub),
                            f.hub.lightning?.let { "Lightning" },
                            f.hub.deposits?.let { "takes Bitcoin" },
                        ).joinToString(" · "),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                TextButton(onClick = { adding = f }, modifier = Modifier.testTag("add-found")) { Text("Add") }
            }
        }
    }
    found?.let { f ->
        if (f.silent > 0u) {
            val more = if (f.silent == 1u) "1 more hub is announced but did not answer"
            else "${f.silent} more hubs are announced but did not answer"
            val tor = when {
                ui.tor || f.silentOnion == 0u -> "."
                f.silentOnion == f.silent -> ": ${if (f.silent == 1u) "it is" else "they are"} reached only through Tor, under Privacy."
                else -> ": ${f.silentOnion} of them only through Tor, under Privacy."
            }
            Note(more + tor)
        }
    }
    error?.let { Note(it, error = true) }
    FilledTonalButton(
        enabled = !looking,
        onClick = ::look,
        modifier = Modifier.padding(vertical = 8.dp),
    ) { Text(if (looking) "Looking…" else if (found == null) "Find hubs" else "Look again") }
    adding?.let { f ->
        AlertDialog(
            onDismissRequest = { adding = null },
            title = { Text("Add this hub?") },
            text = {
                Text(
                    "${f.name?.let { "$it\n" } ?: ""}${f.hub.url}\n\nYou will hold notes there and be paid there, " +
                        "as at your other hubs. Add it only if you trust who runs it.",
                )
            },
            confirmButton = {
                TextButton(onClick = {
                    adding = null
                    scope.launch { engine.addHub(f.hub.url).onFailure { error = explain(it) } }
                }) { Text("Add") }
            },
            dismissButton = { TextButton(onClick = { adding = null }) { Text("Cancel") } },
        )
    }
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

/** How far the app follows Bitcoin, in a word and in a sentence. */
private fun status(ui: Ui): Pair<String, String> {
    val chain = ui.chain
    return when {
        chain.peers == 0L -> "Connecting" to "Looking for Bitcoin nodes."
        !chain.synced -> "Catching up" to "At block ${chain.height}."
        ui.scanned != null && ui.scanned.toInt() <= chain.height ->
            "Looking for payments" to "At block ${ui.scanned} of ${chain.height}."
        else -> "Up to date" to "Block ${chain.height}, ${chain.peers} " + (if (chain.peers == 1L) "node." else "nodes.")
    }
}

/** How far the app follows Bitcoin, in words. */
@Composable
private fun Status(ui: Ui) {
    val (title, detail) = status(ui)
    Column(Modifier.fillMaxWidth().padding(vertical = 12.dp)) {
        Text(title, style = MaterialTheme.typography.bodyLarge)
        Text(detail, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }
    ui.problem?.let { Note(it, error = true) }
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
        Action("Balance and notes held", "Where it is, and the notes of each denomination.", Icons.Filled.Info, balance)
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
