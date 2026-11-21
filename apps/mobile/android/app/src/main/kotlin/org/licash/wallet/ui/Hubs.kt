package org.licash.wallet.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import org.licash.core.FeesRaised
import org.licash.core.FoundHub
import org.licash.core.Hub
import org.licash.core.HubsFound
import org.licash.core.NoteOptimization
import org.licash.wallet.Engine
import org.licash.wallet.Ui
import org.licash.wallet.explain
import org.licash.wallet.digits
import org.licash.wallet.feeOf
import org.licash.wallet.percent
import org.licash.wallet.sat

/*
 * Hubs, wherever one is chosen or looked at: those announced to pick
 * from, one added by its address, and what one charges. Shared by the
 * first run, which has none yet, and by Settings.
 */

/** What a hub is, said where one is chosen. */
const val HUB_MEANS = "A hub holds your balance and moves it for you. Choose one you trust: you can add more later."

/** A hub in a line under its name: its fee on a payment, and what else it does. */
internal fun about(hub: Hub): String = listOfNotNull(
    paying(hub).let { if (it == "None") "No fee" else "Fee $it" },
    hub.lightning?.let { "Lightning" },
    hub.contracts?.let { "Protection" },
).joinToString(" · ")

/** How many announced hubs did not answer, and whether Tor would reach them: nothing if all did. */
internal fun unanswered(found: HubsFound, tor: Boolean): String? {
    if (found.silent == 0u) return null
    val more = if (found.silent == 1u) "1 more did not answer" else "${found.silent} more did not answer"
    return if (!tor && found.silentOnion > 0u) "$more: try with Tor on" else more
}

/**
 * What a hub charges and takes, in a sheet: only what it does. Its address,
 * to copy, `full`. What it `raised` since agreed, if anything, first, to
 * `approve`.
 */
@Composable
internal fun HubFees(
    hub: Hub,
    full: Boolean = false,
    raised: FeesRaised? = null,
    approve: (FeesRaised) -> Unit = {},
    optimize: (() -> Unit)? = null,
    dismiss: () -> Unit,
) {
    val blockSecs = LocalBlockSeconds.current
    Sheet(if (full) name(hub.url) else "Fees and limits", dismiss) {
        if (full) {
            SelectionContainer {
                Text(
                    hub.url,
                    modifier = Modifier.padding(bottom = 8.dp).testTag("hub-url"),
                    style = MaterialTheme.typography.bodySmall,
                    fontFamily = FontFamily.Monospace,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
        raised?.let { r ->
            Group("Raised fees") {
                raises(r).forEachIndexed { i, raise ->
                    if (i > 0) Divider()
                    Entry(raise.what, value = raise.now, sub = "Was ${raise.agreed}", tag = "raised")
                }
            }
            Problem(RAISED_MEANS, error = false)
            BigButton("Approve fees", Modifier.fillMaxWidth().testTag("approve-fees")) { approve(r) }
        }
        // What a payment costs, a section a direction: limits under each.
        Group("Paying") {
            Entry("In Licash", value = paying(hub), tag = "fee-pay")
            val most = hub.lightningPay
            if (hub.lightning != null || most != null) {
                Divider()
                Entry(
                    "Over Lightning",
                    value = hub.lightning?.let { charge(it.pay) } ?: "Yes",
                    sub = most?.let { "Up to ${sat(it)}" },
                )
            }
        }
        if (hub.lightning != null || hub.deposits != null) {
            Group("Receiving") {
                hub.lightning?.let {
                    Entry("Over Lightning", value = charge(it.receive), sub = between(it.receiveRange.min, it.receiveRange.max))
                }
                hub.deposits?.let { range ->
                    if (hub.lightning != null) Divider()
                    Entry(
                        "On Bitcoin",
                        value = hub.depositFee?.let { charge(it) } ?: "Yes",
                        sub = between(range.min, range.max),
                    )
                }
            }
        }
        hub.contracts?.let {
            Group("Protection") {
                // Terms chosen within its range, priced by cap and length;
                // else its one fixed contract.
                val range = it.range
                if (range != null) {
                    Entry(
                        "Price",
                        value = "${sat(range.reserve + range.fee)} + ${percent(range.ppmYear.toULong())} a year",
                        sub = "${sat(range.fee)} opening fee and ${sat(range.reserve)} reserve, returned; " +
                            "interest on the part the hub funds" +
                            if (range.ppmYearFunded == 0u) ", none on yours" else
                                ", ${percent(range.ppmYearFunded.toULong())} a year on yours",
                        tag = "protection-price",
                    )
                    Divider()
                    Entry(
                        "Covers",
                        value = between(range.minCap, range.maxCap),
                        sub = "In sizes doubling from the least, up to ${range.perCoin} times the coin paying for it",
                        tag = "protection-cap",
                    )
                    Divider()
                    Entry(
                        "For",
                        value = "${span(range.minBlocks.toLong(), blockSecs)} to ${span(range.maxBlocks.toLong(), blockSecs)}",
                        tag = "protection-for",
                    )
                } else {
                    Entry("Price", value = feeOf(it.price, it.cap))
                    Divider()
                    Entry("Covers up to", value = sat(it.cap), tag = "protection-cap")
                    Divider()
                    Entry("For", value = span(it.blocks.toLong(), blockSecs))
                }
            }
        }
        if (full) {
            Group("Held payments") {
                Entry("Holding one", value = charge(hub.lockFee))
                Divider()
                Entry("Refunding one", value = charge(hub.refundFee))
            }
        }
        optimize?.let { action ->
            TextButton(onClick = action, modifier = Modifier.testTag("optimize-notes")) { Text("Optimize notes") }
        }
    }
}

/** Explicit optimization, with the shown fee as a hard execution cap. */
@Composable
internal fun OptimizeNotes(engine: Engine, hub: Hub, dismiss: () -> Unit) {
    val scope = rememberCoroutineScope()
    var quote by remember(hub.url) { mutableStateOf<NoteOptimization?>(null) }
    var loading by remember(hub.url) { mutableStateOf(true) }
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    LaunchedEffect(hub.url) {
        engine.optimizationQuote(hub.url).onSuccess { quote = it }.onFailure { error = explain(it) }
        loading = false
    }
    val wait = rememberWait(loading || busy)
    Sheet("Optimize notes", dismiss, dismissable = !busy) {
        Say("Smaller notes leave receiving room and help recovery fit its note limit. Automatic shaping normally handles this.")
        Say("Doing it manually bypasses privacy rest and the daily fee budget, but not the fees you agreed to.")
        when {
            loading -> Say("Checking the notes…")
            quote == null && error == null -> Say("No optimization is available, or one is still pending.")
            else -> quote?.let { q ->
                Line("Notes", "${q.notesBefore} to ${q.notesAfter}")
                Line("Reminting", sat(q.value))
                Line("Fee", feeOf(q.fee, q.value))
                if (q.id != null) {
                    Say("Optimized. Its fee is recorded in History.")
                } else {
                    BigButton("Optimize for ${sat(q.fee)}", Modifier.fillMaxWidth().testTag("confirm-optimize"), enabled = !busy) {
                        busy = true
                        error = null
                        scope.launch {
                            engine.optimizeNotes(q).onSuccess { quote = it }.onFailure { error = explain(it) }
                            busy = false
                        }
                    }
                }
            }
        }
        Waited(wait)
        error?.let { Problem(it) }
        TextButton(onClick = dismiss, enabled = !busy) { Text("Done") }
    }
}

/** From `min` to `max`, in sat: a range a hub takes. */
private fun between(min: ULong, max: ULong): String = "${digits(min)} to ${sat(max)}"

/** A hub added by its address, typed or pasted: `action` names the button. */
@Composable
internal fun AddHub(engine: Engine, action: String = "Add") {
    val scope = rememberCoroutineScope()
    var url by remember { mutableStateOf("") }
    var adding by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    Row(
        Modifier.fillMaxWidth().padding(vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        OutlinedTextField(
            value = url,
            onValueChange = { url = it },
            modifier = Modifier.weight(1f).testTag("hub-address"),
            singleLine = true,
            label = { Text("Hub address") },
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri, imeAction = ImeAction.Done),
            shape = RoundedCornerShape(14.dp),
        )
        FilledTonalButton(
            enabled = url.isNotBlank() && !adding,
            onClick = {
                adding = true
                scope.launch {
                    engine.addHub(url.trim()).onSuccess { url = ""; error = null }.onFailure { error = explain(it) }
                    adding = false
                }
            },
            modifier = Modifier.testTag("add-address"),
        ) { Text(if (adding) "…" else action) }
    }
    Waited(rememberWait(adding))
    error?.let { Problem(it) }
}

/**
 * The hubs announced on Nostr, on the relays the hubs used name and on the
 * default ones, to pick from: a tap shows one, and adds it once the owner
 * says so. Looked for at once if `auto`, else once asked. Those announced
 * but not answering are only counted.
 */
@Composable
internal fun FoundHubs(engine: Engine, ui: Ui, auto: Boolean) {
    val scope = rememberCoroutineScope()
    var found by remember { mutableStateOf<HubsFound?>(null) }
    var looking by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    var adding by remember { mutableStateOf<FoundHub?>(null) }
    // The one being added, once picked, then the last added.
    var joining by remember { mutableStateOf<String?>(null) }
    var joined by remember { mutableStateOf<String?>(null) }
    val lookWait = rememberWait(looking)
    val joinWait = rememberWait(joining != null)
    fun look() {
        looking = true
        error = null
        scope.launch {
            engine.findHubs().onSuccess { found = it }.onFailure { error = explain(it) }
            looking = false
        }
    }
    if (auto) LaunchedEffect(Unit) { look() }
    // Those added since looking drop out.
    val shown = found?.hubs?.filter { f -> ui.hubs.none { sameHub(it.url, f.hub.url) } }
    shown?.forEach { f ->
        Entry(
            f.name ?: name(f.hub.url),
            modifier = Modifier.testTag("add-found"),
            sub = about(f.hub),
            onClick = { adding = f },
        )
        Divider()
    }
    if (shown?.isEmpty() == true) Entry("None found", tag = "none-found")
    found?.let { unanswered(it, ui.tor) }?.let { Problem(it, error = false) }
    error?.let { Problem(it) }
    TextButton(enabled = !looking, onClick = ::look, modifier = Modifier.testTag("find-hubs")) {
        Text(if (looking) "Looking…" else if (found == null) "Find hubs" else "Look again")
    }
    Waited(lookWait, Modifier.padding(start = 12.dp))
    (joining?.let { "Adding $it…" } ?: joined?.let { "Added $it" })?.let {
        Text(it, Modifier.padding(start = 12.dp, top = 8.dp).testTag("joining"), style = MaterialTheme.typography.bodyMedium)
        Waited(joinWait, Modifier.padding(start = 12.dp))
    }
    adding?.let { f ->
        Sheet(f.name ?: name(f.hub.url), dismiss = { adding = null }) {
            Text(
                f.hub.url,
                modifier = Modifier.padding(bottom = 8.dp).testTag("found-url"),
                style = MaterialTheme.typography.bodySmall,
                fontFamily = FontFamily.Monospace,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Line("Paying", paying(f.hub))
            Say("Add it only if you trust who runs it.")
            BigButton("Add hub", Modifier.fillMaxWidth().testTag("add-hub")) {
                adding = null
                val named = f.name ?: name(f.hub.url)
                joining = named
                joined = null
                scope.launch {
                    engine.addHub(f.hub.url).onSuccess { joined = named }.onFailure { error = explain(it) }
                    joining = null
                }
            }
        }
    }
}

/** Something that went wrong, or to know, in a line under what it is about. */
@Composable
internal fun Problem(text: String, error: Boolean = true) {
    Text(
        text,
        modifier = Modifier.padding(top = 8.dp, bottom = 12.dp),
        style = MaterialTheme.typography.bodySmall,
        color = if (error) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurfaceVariant,
    )
}
