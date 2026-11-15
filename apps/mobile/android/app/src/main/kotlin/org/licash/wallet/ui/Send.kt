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
import androidx.compose.material3.CircularProgressIndicator
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
import org.licash.core.InvoiceQuote
import org.licash.core.LicashException
import org.licash.core.PayoutQuote
import org.licash.core.Scanned
import org.licash.core.State
import org.licash.wallet.Engine
import org.licash.wallet.Ui
import org.licash.wallet.explain
import org.licash.wallet.host
import org.licash.wallet.sat

private sealed interface Step {
    data object Looking : Step
    data class Paying(val code: String, val asked: ULong?) : Step
    data class Invoice(val text: String, val quote: InvoiceQuote) : Step
    data class Address(val address: String) : Step
    data class Payout(val quote: PayoutQuote) : Step
    data object Busy : Step
    data object Quoting : Step
    data class PayingOut(val amount: ULong, val depth: UInt) : Step
    data class Sent(val amount: ULong, val handover: String?) : Step
    data class Paid(val amount: ULong, val fee: ULong) : Step
    data object Later : Step
    data class Received(val amount: ULong) : Step
    data class Trust(val payment: String, val hub: String) : Step
    data class Failed(val why: String) : Step
}

/**
 * Scan or paste: a code to pay, or a payment handed over to take; with
 * `cashOut`, a hub, a Bitcoin address to pay all the notes there out to.
 */
@Composable
fun Send(engine: Engine, ui: Ui, link: String?, cashOut: String? = null, back: () -> Unit) {
    val scope = rememberCoroutineScope()
    val clipboard = LocalClipboard.current
    // Opened with a link: no camera to start.
    var step by remember { mutableStateOf<Step>(if (link == null) Step.Looking else Step.Busy) }
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

    suspend fun quoting(address: String, amount: ULong?): Step {
        step = Step.Quoting
        return engine.payoutQuote(address, amount, cashOut).fold(
            { Step.Payout(it) },
            { Step.Failed(explain(it)) },
        )
    }

    fun quote(address: String, amount: ULong?) {
        scope.launch { step = quoting(address, amount) }
    }

    fun read(text: String) {
        step = Step.Busy
        scope.launch {
            step = engine.scanned(text).fold(
                { scanned ->
                    when {
                        // Cashing out: all to an address, nothing else.
                        cashOut != null -> when (scanned) {
                            is Scanned.Bitcoin -> quoting(scanned.address, null)
                            else -> Step.Failed("To cash out, scan or paste a Bitcoin address.")
                        }
                        scanned is Scanned.Code -> when {
                            scanned.notes -> Step.Paying(text, scanned.amount)
                            else -> Step.Failed("This code names no hub you use, and no Bitcoin address to pay.")
                        }
                        scanned is Scanned.Bitcoin -> when (val asked = scanned.amount) {
                            null -> Step.Address(scanned.address)
                            else -> quoting(scanned.address, asked)
                        }
                        scanned is Scanned.Payment -> taking(text, false)
                        scanned is Scanned.Invoice -> engine.invoiceQuote(text).fold(
                            { Step.Invoice(text, it) },
                            { Step.Failed(explain(it)) },
                        )
                        else -> Step.Failed("Nothing to pay there.")
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
                { e -> if (e is LicashException.Pending) Step.Later else Step.Failed(explain(e)) },
            )
        }
    }

    fun payInvoice(text: String, hub: String) {
        step = Step.Busy
        scope.launch {
            step = engine.payInvoice(text, hub).fold(
                { if (it.state == State.DONE) Step.Paid(it.amount, it.fee) else Step.Later },
                { Step.Failed(explain(it)) },
            )
        }
    }

    fun payOut(quote: PayoutQuote) {
        step = Step.Busy
        scope.launch {
            step = engine.payOut(quote).fold(
                { Step.PayingOut(it.amount, quote.depth) },
                { Step.Failed(explain(it)) },
            )
        }
    }

    LaunchedEffect(link) { link?.let { read(it) } }
    Column(Modifier.fillMaxSize().padding(horizontal = 24.dp)) {
        TopBar(if (cashOut == null) "Send" else "Cash out", back)
        when (val s = step) {
            Step.Looking -> {
                if (cashOut != null) {
                    Text(
                        "All you hold at ${host(cashOut)}, paid out on Bitcoin. Scan or paste the address to pay.",
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Spacer(Modifier.height(12.dp))
                }
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
            Step.Quoting -> Middle("Finding a provider to pay it on Bitcoin…")
            is Step.Invoice -> Invoice(s.quote) { payInvoice(s.text, s.quote.hub) }
            is Step.Address -> ToAddress(s.address, asking, { asking = it }) { amount -> quote(s.address, amount) }
            is Step.Payout -> Payout(s.quote) { payOut(s.quote) }
            is Step.PayingOut -> Told(
                "Sending ${sat(s.amount)} on Bitcoin. It reaches the address in about " +
                    "${(s.depth.toInt() + 1) * 10} minutes; your history shows how far it got.",
                back,
            )
            is Step.Paid -> Done("Paid ${sat(s.amount)}, ${sat(s.fee)} in fees", back)
            is Step.Sent -> Sent(s, back)
            // Not to pay again: it finishes by itself.
            Step.Later -> Told("Under way: it finishes by itself, and reaches them then.", back)
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
                if (it == 0uL) "No fee" else "Fee up to ${sat(it)}",
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

/** A Lightning invoice to pay: what it asks, and all it costs on top. */
@Composable
private fun Invoice(quote: InvoiceQuote, pay: () -> Unit) {
    Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.CenterHorizontally) {
        Spacer(Modifier.height(48.dp))
        Text(sat(quote.amount), style = Figures)
        Text(
            if (quote.fee == 0uL) "No fee" else "Fee ${sat(quote.fee)}, ${sat(quote.amount + quote.fee)} in all",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        quote.description?.let {
            Spacer(Modifier.height(12.dp))
            Text(it, style = MaterialTheme.typography.bodyLarge, textAlign = TextAlign.Center)
        }
        Spacer(Modifier.height(8.dp))
        Text(
            "Over Lightning, through ${host(quote.hub)}",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.height(32.dp))
        BigButton("Pay", Modifier.fillMaxWidth(), onClick = pay)
    }
}

/** A Bitcoin address to pay: how much, or all. */
@Composable
private fun ToAddress(address: String, asking: Boolean, ask: (Boolean) -> Unit, quote: (ULong?) -> Unit) {
    var amount by remember { mutableStateOf<Long?>(null) }
    Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.CenterHorizontally) {
        Spacer(Modifier.height(48.dp))
        Text(amount?.let { sat(it.toULong()) } ?: "How much?", style = Figures)
        Text(
            "To ${shortAddress(address)}, on Bitcoin",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.height(32.dp))
        OutlinedButton(onClick = { ask(true) }, modifier = Modifier.fillMaxWidth().height(56.dp)) {
            Text(if (amount == null) "Enter amount" else "Change amount")
        }
        Spacer(Modifier.height(12.dp))
        OutlinedButton(onClick = { quote(null) }, modifier = Modifier.fillMaxWidth().height(56.dp)) {
            Text("Send all")
        }
        Spacer(Modifier.height(12.dp))
        BigButton("Continue", Modifier.fillMaxWidth(), enabled = amount != null) {
            amount?.let { quote(it.toULong()) }
        }
    }
    if (asking) AmountDialog(amount, onDone = { amount = it; ask(false) }, onCancel = { ask(false) })
}

/** A payout on Bitcoin to confirm: what the address gets, and all it costs on top. */
@Composable
private fun Payout(quote: PayoutQuote, pay: () -> Unit) {
    Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.CenterHorizontally) {
        Spacer(Modifier.height(48.dp))
        Text(sat(quote.amount), style = Figures)
        Text(
            "Fees ${sat(quote.fee)}, ${sat(quote.amount + quote.fee)} in all",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.height(12.dp))
        Text(
            "To ${shortAddress(quote.address)}, on Bitcoin",
            style = MaterialTheme.typography.bodyLarge,
            textAlign = TextAlign.Center,
        )
        Spacer(Modifier.height(8.dp))
        Text(
            "From Licash at ${host(quote.hub)}, through ${quote.providerName ?: host(quote.provider)}. " +
                "Arrives in about ${(quote.depth.toInt() + 1) * 10} minutes.",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
        )
        Spacer(Modifier.height(32.dp))
        BigButton("Send", Modifier.fillMaxWidth(), onClick = pay)
    }
}

/** A long address, its ends shown. */
private fun shortAddress(address: String): String =
    if (address.length <= 20) address else address.take(10) + "…" + address.takeLast(8)

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
        CircularProgressIndicator()
        Spacer(Modifier.height(24.dp))
        Text(text, style = MaterialTheme.typography.titleMedium, textAlign = TextAlign.Center)
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
private fun Told(text: String, back: () -> Unit) {
    Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.CenterHorizontally) {
        Spacer(Modifier.height(80.dp))
        Text(text, style = MaterialTheme.typography.bodyLarge, textAlign = TextAlign.Center)
        Spacer(Modifier.height(40.dp))
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
