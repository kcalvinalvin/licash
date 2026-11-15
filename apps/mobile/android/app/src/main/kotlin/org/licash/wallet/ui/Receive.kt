package org.licash.wallet.ui

import android.content.ClipData
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
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
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
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.testTagsAsResourceId
import androidx.compose.ui.text.input.KeyboardType
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

/** How long Copy says it copied, in ms. */
private const val COPIED_MS = 2_000L

/**
 * What to be paid at, as a QR code and one Copy: the wallet's code, paid in
 * Licash or on Bitcoin alike, its fallback and silent payment addresses in
 * it; or instead a plain Lightning invoice, from a hub taking payments over
 * Lightning.
 */
@Composable
fun Receive(engine: Engine, back: () -> Unit) {
    val clipboard = LocalClipboard.current
    val scope = rememberCoroutineScope()
    var amount by rememberSaveable { mutableStateOf<Long?>(null) }
    var code by remember { mutableStateOf<Code?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    var asking by remember { mutableStateOf(false) }
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
            Spacer(Modifier.height(16.dp))
            received?.let {
                Text(
                    it,
                    style = MaterialTheme.typography.headlineSmall,
                    color = MaterialTheme.colorScheme.primary,
                    textAlign = TextAlign.Center,
                )
                Spacer(Modifier.height(16.dp))
            }
            // In upper case, which a QR code holds in fewer modules.
            when {
                lightning && request != null -> QrCode(request.uppercase())
                lightning && amount != null && error == null -> Waiting()
                !lightning -> code?.let { QrCode(it.qr) } ?: Waiting()
            }
            error?.let {
                Spacer(Modifier.height(8.dp))
                Text(it, color = MaterialTheme.colorScheme.error, textAlign = TextAlign.Center)
            }
            Spacer(Modifier.height(16.dp))
            Text(
                amount?.let { "Asking for ${sat(it.toULong())}" + if (lightning) " over Lightning" else "" }
                    ?: if (lightning) "Choose an amount" else "Any amount",
                style = MaterialTheme.typography.titleMedium,
            )
            Text(
                when {
                    !lightning -> "Anyone can pay this, with Licash or on Bitcoin."
                    amount == null -> "A Lightning invoice asks for an amount."
                    request != null -> "Any Lightning wallet can pay this, once." +
                        (invoice?.asks?.let { " The payer pays ${sat(it)}, the hub's fee included." } ?: "")
                    invoice?.reason != null -> "Waiting for the hub: ${invoice?.reason}"
                    else -> "Asking the hub for an invoice"
                },
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                textAlign = TextAlign.Center,
            )
            if (ui.hubs.any { it.lightning != null }) {
                Spacer(Modifier.height(8.dp))
                TextButton(onClick = {
                    lightning = !lightning
                    error = null
                    // An invoice asks for an amount.
                    if (lightning && amount == null) asking = true
                }) { Text(if (lightning) "Receive with Licash or Bitcoin" else "Receive over Lightning") }
            }
        }
        Row(Modifier.padding(vertical = 24.dp), horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            OutlinedButton(onClick = { asking = true }, modifier = Modifier.weight(1f).height(56.dp)) {
                Text(if (amount == null) "Amount" else "Change amount")
            }
            BigButton(if (copied) "Copied" else "Copy", Modifier.weight(1f), enabled = shown != null) {
                shown?.let { (label, text) ->
                    val clip = ClipData.newPlainText(label, text)
                    scope.launch { clipboard.setClipEntry(ClipEntry(clip)) }
                    copied = true
                }
            }
        }
    }
    if (asking) {
        AmountDialog(amount, onDone = { amount = it; asking = false }, onCancel = { asking = false })
    }
}

/** Where a QR code shows once made. */
@Composable
private fun Waiting() {
    Box(Modifier.size(316.dp), contentAlignment = Alignment.Center) { CircularProgressIndicator() }
}

@Composable
fun AmountDialog(current: Long?, onDone: (Long?) -> Unit, onCancel: () -> Unit) {
    var text by remember { mutableStateOf(current?.toString() ?: "") }
    AlertDialog(
        onDismissRequest = onCancel,
        // A window of its own: its test tags as resource ids too.
        modifier = Modifier.semantics { testTagsAsResourceId = true },
        title = { Text("Amount in sat") },
        text = {
            OutlinedTextField(
                value = text,
                onValueChange = { text = it.filter(Char::isDigit).take(15) },
                modifier = Modifier.testTag("amount"),
                singleLine = true,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
            )
        },
        confirmButton = { TextButton(onClick = { onDone(text.toLongOrNull()?.takeIf { it > 0 }) }) { Text("Done") } },
        dismissButton = { TextButton(onClick = onCancel) { Text("Cancel") } },
    )
}
