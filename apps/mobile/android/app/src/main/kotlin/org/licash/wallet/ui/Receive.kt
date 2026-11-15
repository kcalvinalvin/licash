package org.licash.wallet.ui

import android.content.ClipData
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
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
import androidx.compose.ui.platform.ClipEntry
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import org.licash.core.Code
import org.licash.core.LightningInvoice
import org.licash.core.State
import org.licash.wallet.Engine
import org.licash.wallet.explain
import org.licash.wallet.sat

/** How often an invoice shown is followed, in ms. */
private const val FOLLOW_MS = 3_000L

/** How long copying says it copied, in ms. */
internal const val COPIED_MS = 2_000L

/**
 * What to be paid at, as a QR code and one Copy: the wallet's code, paid in
 * Licash or on Bitcoin alike, its fallback and silent payment addresses in
 * it; or instead a plain Lightning invoice, from a hub taking payments over
 * Lightning, one switch away. Under it, the amount asked and nothing else.
 */
@Composable
fun Receive(engine: Engine, back: () -> Unit) {
    val clipboard = LocalClipboard.current
    val scope = rememberCoroutineScope()
    var amount by rememberSaveable { mutableStateOf<Long?>(null) }
    var code by remember { mutableStateOf<Code?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    // The amount typed in, in place of the QR code: asked for once done.
    var editing by remember { mutableStateOf(false) }
    var typed by remember { mutableStateOf<Long?>(null) }
    val edit = {
        typed = amount
        editing = true
    }
    val done = {
        amount = typed
        editing = false
    }
    var received by remember { mutableStateOf<String?>(null) }
    var copied by remember { mutableStateOf(false) }
    val ui by engine.ui.collectAsState()
    // An invoice instead, made by a hub: for an amount, paid once.
    var lightning by rememberSaveable { mutableStateOf(false) }
    var invoice by remember { mutableStateOf<LightningInvoice?>(null) }
    val request = invoice?.request?.takeIf { lightning }
    val shown = when {
        lightning -> request?.let { "Lightning invoice" to it }
        else -> code?.let { "Licash code" to it.text }
    }
    // Made again once paid: a fallback address paid gives way to the next.
    LaunchedEffect(amount, received) {
        code = null
        error = null
        engine.code(amount?.toULong())
            .onSuccess { code = it }
            .onFailure { error = explain(it) }
    }
    LaunchedEffect(lightning, amount) {
        invoice = null
        val asked = amount?.takeIf { lightning } ?: return@LaunchedEffect
        error = null
        engine.askInvoice(asked.toULong())
            .onSuccess { invoice = it }
            .onFailure { error = explain(it) }
    }
    // Followed while shown: its notes taken once it is paid.
    LaunchedEffect(invoice?.hash) {
        val hash = invoice?.hash ?: return@LaunchedEffect
        while (true) {
            delay(FOLLOW_MS)
            val followed = engine.followInvoice(hash).getOrNull() ?: continue
            invoice = followed
            when (followed.state) {
                State.DONE -> {
                    received = "Received ${sat(followed.amount)}"
                    amount = null
                    return@LaunchedEffect
                }
                State.FAILED -> {
                    error = "Not paid: ${followed.reason ?: "canceled"}"
                    return@LaunchedEffect
                }
                State.PENDING -> {}
            }
        }
    }
    LaunchedEffect(copied) {
        if (copied) {
            delay(COPIED_MS)
            copied = false
        }
    }
    DisposableEffect(Unit) {
        engine.eager = true
        engine.poke()
        onDispose { engine.eager = false }
    }
    LaunchedEffect(Unit) {
        engine.events.collect { event -> if (event.received != null) received = event.text }
    }
    Column(Modifier.fillMaxSize().padding(horizontal = 24.dp)) {
        TopBar("Receive", back)
        Column(
            Modifier.fillMaxWidth().weight(1f).verticalScroll(rememberScrollState()),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            if (ui.hubs.any { it.lightning != null }) {
                Segmented(listOf("Bitcoin", "Lightning"), if (lightning) 1 else 0, Modifier.fillMaxWidth()) { i ->
                    lightning = i == 1
                    error = null
                    // An invoice asks for an amount.
                    if (lightning && amount == null) edit() else editing = false
                }
            }
            Spacer(Modifier.height(16.dp))
            received?.let {
                Text(
                    it,
                    modifier = Modifier.testTag("received"),
                    style = MaterialTheme.typography.headlineSmall,
                    color = MaterialTheme.colorScheme.primary,
                    textAlign = TextAlign.Center,
                )
                Spacer(Modifier.height(16.dp))
            }
            // In upper case, which a QR code holds in fewer modules.
            when {
                editing -> {
                    Spacer(Modifier.height(32.dp))
                    AmountField(typed, { typed = it }) { done() }
                    Spacer(Modifier.height(16.dp))
                }
                lightning && request != null -> QrCode(request.uppercase())
                lightning && amount != null && error == null -> Waiting()
                !lightning -> when (val made = code) {
                    null -> if (error == null) Waiting()
                    else -> QrCode(made.qr)
                }
            }
            error?.let {
                Spacer(Modifier.height(8.dp))
                Text(it, color = MaterialTheme.colorScheme.error, textAlign = TextAlign.Center)
            }
            Spacer(Modifier.height(16.dp))
            if (!editing) {
                Text(
                    amount?.let { sat(it.toULong()) } ?: if (lightning) "Choose an amount" else "Any amount",
                    modifier = Modifier.testTag("asking"),
                    style = MaterialTheme.typography.titleMedium,
                )
            }
            // What the payer pays on top, and what the hub is waiting for: nothing else.
            val asks = invoice?.asks?.takeIf { request != null && it != amount?.toULong() }
            val waits = invoice?.reason?.takeIf { lightning && amount != null && request == null }
            (asks?.let { "The payer pays ${sat(it)}" } ?: waits)?.let {
                Text(
                    it,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    textAlign = TextAlign.Center,
                )
            }
        }
        Row(Modifier.padding(vertical = 24.dp), horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            if (editing) {
                BigButton("Cancel", Modifier.weight(1f), quiet = true) { editing = false }
                BigButton("Done", Modifier.weight(1f).testTag("done")) { done() }
            } else {
                BigButton("Amount", Modifier.weight(1f).testTag("set-amount"), quiet = true, onClick = edit)
                BigButton(if (copied) "Copied" else "Copy", Modifier.weight(1f).testTag("copy"), enabled = shown != null) {
                    shown?.let { (label, text) ->
                        val clip = ClipData.newPlainText(label, text)
                        scope.launch { clipboard.setClipEntry(ClipEntry(clip)) }
                        copied = true
                    }
                }
            }
        }
    }
}

/** Where a QR code shows once made. */
@Composable
private fun Waiting() {
    Box(
        Modifier.widthIn(max = 316.dp).fillMaxWidth().aspectRatio(1f),
        contentAlignment = Alignment.Center,
    ) { CircularProgressIndicator() }
}
