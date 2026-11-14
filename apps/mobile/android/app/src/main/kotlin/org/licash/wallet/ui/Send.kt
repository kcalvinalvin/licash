package org.licash.wallet.ui

import android.content.Intent
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import org.licash.core.LicashException
import org.licash.core.Scanned
import org.licash.wallet.Engine
import org.licash.wallet.Ui
import org.licash.wallet.explain
import org.licash.wallet.sat

private sealed interface Step {
    data object Looking : Step
    data class Paying(val code: String, val asked: ULong?) : Step
    data object Busy : Step
    data class Sent(val amount: ULong, val handover: String?) : Step
    data class Received(val amount: ULong) : Step
    data class Trust(val payment: String, val hub: String) : Step
    data class Failed(val why: String) : Step
}

/** Scan or paste: a code to pay, or a payment handed over to take. */
@Composable
fun Send(engine: Engine, ui: Ui, link: String?, back: () -> Unit) {
    val scope = rememberCoroutineScope()
    val clipboard = LocalClipboard.current
    var step by remember { mutableStateOf<Step>(Step.Looking) }
    var asking by remember { mutableStateOf(false) }

    suspend fun taking(payment: String, trust: Boolean): Step =
        engine.receive(payment, trust).fold(
            { Step.Received(it.amount) },
            { e ->
                when (e) {
                    is LicashException.UnknownHub -> Step.Trust(payment, e.url)
                    else -> Step.Failed(explain(e))
                }
            },
        )

    fun take(payment: String, trust: Boolean) {
        step = Step.Busy
        scope.launch { step = taking(payment, trust) }
    }

    fun read(text: String) {
        step = Step.Busy
        scope.launch {
            step = engine.scanned(text).fold(
                { scanned ->
                    when (scanned) {
                        is Scanned.Code -> when {
                            scanned.notes -> Step.Paying(text, scanned.amount)
                            else -> Step.Failed(
                                "This code names no hub you use: paying it on Bitcoin is not in the app yet.",
                            )
                        }
                        is Scanned.Payment -> taking(text, false)
                    }
                },
                { Step.Failed(explain(it)) },
            )
        }
    }

    fun pay(code: String, amount: ULong?) {
        step = Step.Busy
        scope.launch {
            step = engine.pay(code, amount).fold(
                { Step.Sent(it.amount, it.handover) },
                { Step.Failed(explain(it)) },
            )
        }
    }

    LaunchedEffect(link) { link?.let { read(it) } }
    Column(Modifier.fillMaxSize().padding(horizontal = 24.dp)) {
        TopBar("Send", back)
        when (val s = step) {
            Step.Looking -> {
                Scanner(Modifier.fillMaxWidth().aspectRatio(1f).clip(RoundedCornerShape(24.dp))) { read(it) }
                Spacer(Modifier.height(16.dp))
                OutlinedButton(
                    onClick = {
                        scope.launch {
                            val clip = clipboard.getClipEntry()?.clipData
                            clip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.text?.let { read(it.toString()) }
                        }
                    },
                    modifier = Modifier.fillMaxWidth().height(56.dp),
                ) { Text("Paste") }
            }
            is Step.Paying -> Paying(ui, s, asking, { asking = it }) { amount -> pay(s.code, amount) }
            Step.Busy -> Middle("One moment…")
            is Step.Sent -> Sent(s, back)
            is Step.Received -> Done("Received ${sat(s.amount)}", back)
            is Step.Trust -> Ask(
                "This payment is at a hub you don't use yet:\n${s.hub}\n\nTake it, and use that hub from now on?",
                "Take it",
                { take(s.payment, true) },
                back,
            )
            is Step.Failed -> Ask(s.why, "Try again", { step = Step.Looking }, back)
        }
    }
}

@Composable
private fun Paying(ui: Ui, s: Step.Paying, asking: Boolean, ask: (Boolean) -> Unit, pay: (ULong?) -> Unit) {
    var amount by remember { mutableStateOf(s.asked?.toLong()) }
    // The most a hub would charge: the code may name several.
    val fee = amount?.let { a -> ui.hubs.maxOfOrNull { it.feeBase + it.feePpm * a.toULong() / 1_000_000uL } }
    Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.CenterHorizontally) {
        Spacer(Modifier.height(48.dp))
        Text(amount?.let { sat(it.toULong()) } ?: "How much?", style = Figures)
        fee?.let {
            Text(
                "Fee up to ${sat(it)}",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        Spacer(Modifier.height(32.dp))
        if (s.asked == null) {
            OutlinedButton(onClick = { ask(true) }, modifier = Modifier.fillMaxWidth().height(56.dp)) {
                Text(if (amount == null) "Enter amount" else "Change amount")
            }
            Spacer(Modifier.height(12.dp))
        }
        BigButton("Pay", Modifier.fillMaxWidth(), enabled = amount != null) {
            pay(if (s.asked == null) amount?.toULong() else null)
        }
    }
    if (asking) AmountDialog(amount, onDone = { amount = it; ask(false) }, onCancel = { ask(false) })
}

/** Paid; if not to the payee's mailbox, what to hand them. */
@Composable
private fun Sent(s: Step.Sent, back: () -> Unit) {
    val context = LocalContext.current
    if (s.handover == null) {
        Done("Sent ${sat(s.amount)}", back)
        return
    }
    Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.CenterHorizontally) {
        Spacer(Modifier.height(32.dp))
        Text("Sent ${sat(s.amount)}", style = MaterialTheme.typography.headlineSmall)
        Text(
            "Their mailbox could not be reached: hand them the payment.",
            style = MaterialTheme.typography.bodyMedium,
            textAlign = TextAlign.Center,
        )
        Spacer(Modifier.height(16.dp))
        if (s.handover.length <= 1500) QrCode(s.handover)
        Spacer(Modifier.height(16.dp))
        BigButton("Share payment", Modifier.fillMaxWidth()) {
            val send = Intent(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_TEXT, s.handover)
            context.startActivity(Intent.createChooser(send, null))
        }
        Spacer(Modifier.height(12.dp))
        OutlinedButton(onClick = back, modifier = Modifier.fillMaxWidth().height(56.dp)) { Text("Done") }
    }
}

@Composable
private fun Middle(text: String) {
    Column(Modifier.fillMaxSize(), horizontalAlignment = Alignment.CenterHorizontally) {
        Spacer(Modifier.height(120.dp))
        Text(text, style = MaterialTheme.typography.titleMedium)
    }
}

@Composable
private fun Done(text: String, back: () -> Unit) {
    Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.CenterHorizontally) {
        Spacer(Modifier.height(120.dp))
        Text("✓", style = Figures, color = MaterialTheme.colorScheme.primary)
        Text(text, style = MaterialTheme.typography.headlineSmall)
        Spacer(Modifier.height(48.dp))
        BigButton("Done", Modifier.fillMaxWidth(), onClick = back)
    }
}

@Composable
private fun Ask(text: String, action: String, act: () -> Unit, back: () -> Unit) {
    Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.CenterHorizontally) {
        Spacer(Modifier.height(80.dp))
        Text(text, style = MaterialTheme.typography.bodyLarge, textAlign = TextAlign.Center)
        Spacer(Modifier.height(40.dp))
        BigButton(action, Modifier.fillMaxWidth(), onClick = act)
        Spacer(Modifier.height(12.dp))
        OutlinedButton(onClick = back, modifier = Modifier.fillMaxWidth().height(56.dp)) { Text("Cancel") }
    }
}
