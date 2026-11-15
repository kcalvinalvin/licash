package org.licash.wallet.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material.icons.filled.Warning
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
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import org.licash.core.Activity
import org.licash.core.Details
import org.licash.core.Kind
import org.licash.core.State
import org.licash.wallet.Engine
import org.licash.wallet.Ui
import org.licash.wallet.at
import org.licash.wallet.credit
import org.licash.wallet.digits
import org.licash.wallet.explain
import org.licash.wallet.host
import org.licash.wallet.key
import org.licash.wallet.sat
import org.licash.wallet.total

@Composable
fun Home(
    engine: Engine,
    ui: Ui,
    receive: () -> Unit,
    send: () -> Unit,
    settings: () -> Unit,
    balance: () -> Unit,
    open: (Activity) -> Unit,
) {
    // One list, the balance and buttons atop the history: all of it
    // scrolls on short screens and with large fonts.
    LazyColumn(Modifier.fillMaxSize().padding(horizontal = 24.dp)) {
        item(key = "top") {
            Column {
                Row(Modifier.fillMaxWidth().padding(top = 8.dp), verticalAlignment = Alignment.CenterVertically) {
                    Wordmark()
                    Spacer(Modifier.width(10.dp))
                    NetworkChip(ui.network)
                    Spacer(Modifier.weight(1f))
                    IconButton(onClick = settings) { Icon(Icons.Default.Settings, contentDescription = "Settings") }
                }
                Spacer(Modifier.height(40.dp))
                Total(ui, balance)
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
            }
        }
        if (ui.history.isEmpty() && ui.incoming.isEmpty()) {
            item(key = "none") {
                Text(
                    "Nothing yet. What you receive and send shows here.",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
        items(ui.incoming, key = { "coming " + it.id }) { activity ->
            ActivityRow(activity, coming = true) { open(activity) }
            HorizontalDivider(color = MaterialTheme.colorScheme.outline.copy(alpha = 0.4f))
        }
        items(ui.history, key = { it.key }) { activity ->
            ActivityRow(activity, details = ui.under[activity.key]) { open(activity) }
            HorizontalDivider(color = MaterialTheme.colorScheme.outline.copy(alpha = 0.4f))
        }
    }
}

@Composable
private fun Total(ui: Ui, open: () -> Unit) {
    val b = ui.balance
    Row(Modifier.clickable(onClick = open), verticalAlignment = Alignment.Bottom) {
        Text(digits(b.total), modifier = Modifier.testTag("balance"), style = Figures)
        Text(
            " sat",
            modifier = Modifier.padding(bottom = 8.dp),
            style = MaterialTheme.typography.titleMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
    // Owed by hubs on trust alone: as plain as it gets.
    if (b.credit > 0uL) {
        Text(
            "${sat(b.credit)} on credit",
            modifier = Modifier.clickable(onClick = open).padding(vertical = 4.dp).testTag("credit"),
            style = MaterialTheme.typography.headlineSmall,
            color = MaterialTheme.colorScheme.error,
        )
    }
    val notes = buildList {
        if (b.moving > 0uL) add("${sat(b.moving)} on its way from Bitcoin")
        if (b.onchain > b.kept) add("${sat(b.onchain - b.kept)} on Bitcoin, moved in once enough")
        if (b.kept > 0uL) add("${sat(b.kept)} cashed out, kept on Bitcoin")
        if (b.covered > 0uL) add("${sat(b.covered)} in notes protected by contracts")
        if (b.opening > 0uL) add("${sat(b.opening)} coming back from opening a contract")
        ui.opening?.let { add("Opening a contract at ${host(it)}…") }
        val coming = ui.incoming.sumOf { it.amount }
        if (coming > 0uL) add("${sat(coming)} coming on Bitcoin, waiting for a block")
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
    Text(
        "Where it is",
        modifier = Modifier.clickable(onClick = open).padding(vertical = 6.dp),
        style = MaterialTheme.typography.labelLarge,
        color = MaterialTheme.colorScheme.primary,
    )
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
                placeholder = { Text("hub.example") },
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

/**
 * A line of the history: what it was, an icon of its kind and direction,
 * and when it was; one under way tells which step it is at, from
 * [details], as its page does.
 */
@Composable
private fun ActivityRow(activity: Activity, coming: Boolean = false, details: Details? = null, open: () -> Unit) {
    val colors = MaterialTheme.colorScheme
    val failed = !coming && activity.state == State.FAILED
    val under = !coming && activity.state == State.PENDING
    val sign = when {
        activity.kind == Kind.SWAP -> ""
        !coming && paid(activity) -> "−"
        else -> "+"
    }
    val detail = when {
        coming -> "Waiting for a block"
        failed -> activity.detail
        // "Under way" itself shows beside the title.
        under -> status(activity, false, details).takeIf { details != null && it != "Under way" }
        else -> activity.at?.let { at(it) } ?: activity.detail
    }
    val tint = when {
        failed -> colors.error
        coming -> colors.onSurfaceVariant
        sign == "+" -> colors.primary
        activity.kind == Kind.SWAP -> colors.secondary
        else -> colors.onSurfaceVariant
    }
    Row(
        Modifier.fillMaxWidth().clickable(onClick = open).padding(vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Surface(Modifier.size(40.dp), shape = CircleShape, color = tint.copy(alpha = 0.14f)) {
            Box(contentAlignment = Alignment.Center) {
                Icon(glyph(activity, coming, failed), contentDescription = null, Modifier.size(20.dp), tint = tint)
            }
        }
        Spacer(Modifier.width(14.dp))
        Column(Modifier.weight(1f)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    title(activity, coming),
                    Modifier.weight(1f, fill = false),
                    style = MaterialTheme.typography.bodyLarge,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                if (under) {
                    Spacer(Modifier.width(8.dp))
                    Surface(shape = RoundedCornerShape(50), color = colors.tertiary.copy(alpha = 0.16f)) {
                        Text(
                            "Under way",
                            Modifier.padding(horizontal = 8.dp, vertical = 2.dp),
                            style = MaterialTheme.typography.labelSmall,
                            color = colors.tertiary,
                        )
                    }
                }
            }
            detail?.let {
                Text(
                    it,
                    style = MaterialTheme.typography.bodySmall,
                    color = if (failed) colors.error else colors.onSurfaceVariant,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
        Spacer(Modifier.width(12.dp))
        Text(
            "$sign${sat(activity.amount)}",
            style = MaterialTheme.typography.bodyLarge,
            color = when {
                failed -> colors.error
                coming -> colors.onSurfaceVariant
                sign == "+" -> colors.primary
                else -> colors.onSurface
            },
        )
    }
}

/** The icon of a line of the history: failed, or its kind and direction. */
private fun glyph(activity: Activity, coming: Boolean, failed: Boolean): ImageVector = when {
    failed -> Icons.Default.Warning
    coming -> Glyphs.down
    else -> when (activity.kind) {
        Kind.LIGHTNING, Kind.INVOICE -> Glyphs.bolt
        Kind.SWAP -> Glyphs.swap
        Kind.CONTRACT -> Icons.Default.Lock
        Kind.SENT, Kind.PAYOUT -> Glyphs.up
        Kind.RECEIVED, Kind.MOVED, Kind.ONCHAIN -> Glyphs.down
    }
}
