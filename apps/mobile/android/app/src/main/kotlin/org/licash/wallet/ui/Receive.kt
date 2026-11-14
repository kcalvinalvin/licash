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
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
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
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import org.licash.core.Code
import org.licash.wallet.Engine
import org.licash.wallet.explain
import org.licash.wallet.sat

/** The wallet's code, to be paid at in Licash or on Bitcoin alike. */
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
    // Made again once paid: a fallback address paid gives way to the next.
    LaunchedEffect(amount, received) {
        engine.code(amount?.toULong())
            .onSuccess { code = it }
            .onFailure { error = explain(it) }
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
            code?.let { QrCode(it.qr) }
            error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
            Spacer(Modifier.height(16.dp))
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
        Row(Modifier.padding(bottom = 24.dp), horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            OutlinedButton(onClick = { asking = true }, modifier = Modifier.weight(1f).height(56.dp)) {
                Text(if (amount == null) "Amount" else "Change")
            }
            OutlinedButton(
                onClick = {
                    code?.let {
                        val clip = ClipData.newPlainText("Licash code", it.text)
                        scope.launch { clipboard.setClipEntry(ClipEntry(clip)) }
                    }
                },
                modifier = Modifier.weight(1f).height(56.dp),
            ) { Text("Copy") }
            BigButton("Share", Modifier.weight(1f)) {
                code?.let {
                    val send = Intent(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_TEXT, it.text)
                    context.startActivity(Intent.createChooser(send, null))
                }
            }
        }
    }
    if (asking) {
        AmountDialog(amount, onDone = { amount = it; asking = false }, onCancel = { asking = false })
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
