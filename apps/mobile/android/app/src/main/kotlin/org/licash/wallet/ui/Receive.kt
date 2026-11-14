package org.licash.wallet.ui

import android.content.ClipData
import android.content.Intent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.AlertDialog
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
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.testTagsAsResourceId
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
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

/**
 * The wallet's code, to be paid at in Licash or on Bitcoin alike; or a
 * Lightning invoice, from a hub taking payments over Lightning.
 */
@Composable
fun Receive(engine: Engine, back: () -> Unit) {
    val context = LocalContext.current
    val clipboard = LocalClipboard.current
    val scope = rememberCoroutineScope()
    var amount by rememberSaveable { mutableStateOf<Long?>(null) }
    var code by remember { mutableStateOf<Code?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    var asking by remember { mutableStateOf(false) }
    var received by remember { mutableStateOf<String?>(null) }
    // Its silent payment address on its own rather than the code, for
    // wallets that pay such addresses only.
    var plain by rememberSaveable { mutableStateOf(false) }
    val silent = code?.silent?.takeIf { plain }
    val ui by engine.ui.collectAsState()
    // An invoice instead, made by a hub: for an amount, paid once.
    var lightning by rememberSaveable { mutableStateOf(false) }
    var invoice by remember { mutableStateOf<LightningInvoice?>(null) }
    val request = invoice?.request?.takeIf { lightning }
    val shared = when {
        lightning -> request?.let { "Lightning invoice" to it }
        silent != null -> "Silent payment address" to silent
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
        Column(Modifier.fillMaxWidth().weight(1f), horizontalAlignment = Alignment.CenterHorizontally) {
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
                lightning -> request?.let { QrCode(it.uppercase()) }
                else -> code?.let { QrCode(silent?.uppercase() ?: it.qr) }
            }
            error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
            Spacer(Modifier.height(16.dp))
            if (lightning) {
                LightningInvoiceText(amount, invoice, request)
            } else if (silent != null) {
                Text("Silent payment address", style = MaterialTheme.typography.titleMedium)
                SelectionContainer {
                    Text(
                        silent,
                        Modifier.testTag("silent"),
                        style = MaterialTheme.typography.bodySmall,
                        fontFamily = FontFamily.Monospace,
                        textAlign = TextAlign.Center,
                    )
                }
                Text(
                    "Wallets that pay silent payment addresses can pay this on Bitcoin, " +
                        "the same every time. It asks for no amount.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    textAlign = TextAlign.Center,
                )
            } else {
                Text(
                    amount?.let { "Asking for ${sat(it.toULong())}" } ?: "Any amount",
                    style = MaterialTheme.typography.titleMedium,
                )
                Text(
                    "Anyone can pay this, with Licash or on Bitcoin.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    textAlign = TextAlign.Center,
                )
                // For exchanges and the like, which take a plain address only.
                code?.address?.let { address ->
                    TextButton(onClick = {
                        val clip = ClipData.newPlainText("Bitcoin address", address)
                        scope.launch { clipboard.setClipEntry(ClipEntry(clip)) }
                    }) { Text("Copy Bitcoin address") }
                }
            }
            // For wallets that pay silent payment addresses: the same one
            // every time, found paid through a hub's tweaks.
            if (!lightning && code?.silent != null) {
                TextButton(onClick = { plain = !plain }) {
                    Text(if (plain) "Show payment code" else "Show silent payment address")
                }
            }
            if (ui.hubs.any { it.lightning != null }) {
                TextButton(onClick = {
                    lightning = !lightning
                    plain = false
                    error = null
                    // An invoice asks for an amount.
                    if (lightning && amount == null) asking = true
                }) { Text(if (lightning) "Show payment code" else "Receive over Lightning") }
            }
        }
        Row(Modifier.padding(bottom = 24.dp), horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            OutlinedButton(onClick = { asking = true }, modifier = Modifier.weight(1f).height(56.dp)) {
                Text(if (amount == null) "Amount" else "Change")
            }
            OutlinedButton(
                onClick = {
                    shared?.let { (label, text) ->
                        val clip = ClipData.newPlainText(label, text)
                        scope.launch { clipboard.setClipEntry(ClipEntry(clip)) }
                    }
                },
                modifier = Modifier.weight(1f).height(56.dp),
            ) { Text("Copy") }
            BigButton("Share", Modifier.weight(1f)) {
                shared?.let { (_, text) ->
                    val send = Intent(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_TEXT, text)
                    context.startActivity(Intent.createChooser(send, null))
                }
            }
        }
    }
    if (asking) {
        // An amount is asked for in the code: shown again, as the address
        // holds none.
        AmountDialog(amount, onDone = { amount = it; asking = false; plain = false }, onCancel = { asking = false })
    }
}

/** What a Lightning invoice shown asks, or why there is none yet. */
@Composable
private fun LightningInvoiceText(amount: Long?, invoice: LightningInvoice?, request: String?) {
    Text(
        amount?.let { "Asking for ${sat(it.toULong())} over Lightning" } ?: "Choose an amount",
        style = MaterialTheme.typography.titleMedium,
    )
    val asks = invoice?.asks?.let { " The payer pays ${sat(it)}, the hub's fee included." } ?: ""
    Text(
        when {
            amount == null -> "A Lightning invoice asks for an amount."
            request != null -> "Any Lightning wallet can pay this, once.$asks"
            invoice?.reason != null -> "Waiting for the hub: ${invoice.reason}"
            else -> "Asking the hub for an invoice"
        },
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        textAlign = TextAlign.Center,
    )
    // Its start, to tell it by: Copy and Share give all of it.
    request?.let {
        Text(
            it,
            Modifier.testTag("invoice"),
            style = MaterialTheme.typography.bodySmall,
            fontFamily = FontFamily.Monospace,
            textAlign = TextAlign.Center,
            maxLines = 2,
            overflow = TextOverflow.Ellipsis,
        )
    }
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
