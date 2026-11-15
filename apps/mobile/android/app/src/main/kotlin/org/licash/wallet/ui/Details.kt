package org.licash.wallet.ui

import android.content.ClipData
import android.content.Intent
import android.net.Uri
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
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
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.ClipEntry
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import org.licash.core.Activity
import org.licash.core.Details
import org.licash.core.Kind
import org.licash.core.Onchain
import org.licash.core.State
import org.licash.wallet.AppLog
import org.licash.wallet.Engine
import org.licash.wallet.Ui
import org.licash.wallet.at
import org.licash.wallet.digits
import org.licash.wallet.explain
import org.licash.wallet.host
import org.licash.wallet.sat

/** A line of the history on its own: what it cost and where it stands. */
@Composable
fun ActivityDetails(engine: Engine, ui: Ui, shown: Activity, back: () -> Unit) {
    val context = LocalContext.current
    val clipboard = LocalClipboard.current
    val scope = rememberCoroutineScope()
    // As it stands now: the history changes while shown.
    val coming = ui.incoming.find { it.id == shown.id }
    val activity = coming ?: ui.history.find { it.id == shown.id && it.kind == shown.kind } ?: shown
    var details by remember { mutableStateOf<Details?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    LaunchedEffect(activity, ui.scanned) {
        engine.details(activity)
            .onSuccess { details = it; error = null }
            .onFailure { error = explain(it) }
    }
    val copy: (String, String) -> Unit = { label, text ->
        scope.launch { clipboard.setClipEntry(ClipEntry(ClipData.newPlainText(label, text))) }
    }
    val tx = when (val d = details) {
        is Details.Moved -> d.payment
        is Details.Coin -> d.tx
        else -> null
    }
    // A payout's claim pays the address: else the provider's coin.
    val txid = tx?.txid ?: (details as? Details.Payout)?.let { it.claim ?: it.coin }
    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(horizontal = 20.dp)) {
        TopBar(title(activity, coming != null), back)
        Spacer(Modifier.height(12.dp))
        val sign = if (paid(activity)) "−" else "+"
        Text(
            "$sign${digits(activity.amount)} sat",
            modifier = Modifier.padding(horizontal = 4.dp),
            style = MaterialTheme.typography.displaySmall,
        )
        Text(
            status(activity, coming != null, details),
            modifier = Modifier.padding(horizontal = 4.dp, vertical = 4.dp),
            style = MaterialTheme.typography.bodyLarge,
            color = if (activity.state == State.FAILED) {
                MaterialTheme.colorScheme.error
            } else {
                MaterialTheme.colorScheme.onSurfaceVariant
            },
        )
        error?.let { Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall) }
        Summary(activity, details, tx)
        txid?.let { engine.explorerLink(it) }?.let { link ->
            Spacer(Modifier.height(16.dp))
            OutlinedButton(
                onClick = {
                    runCatching { context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(link))) }
                        .onFailure { AppLog.warn("opening $link", it) }
                },
                modifier = Modifier.fillMaxWidth().height(52.dp),
            ) { Text("View on a block explorer") }
        }
        if (ui.developer) Technical(activity, details, copy)
        Spacer(Modifier.height(48.dp))
    }
}

private fun title(activity: Activity, coming: Boolean): String = when {
    coming -> "Coming on Bitcoin"
    else -> when (activity.kind) {
        Kind.SENT -> "Sent"
        Kind.RECEIVED -> "Received"
        Kind.MOVED -> "From Bitcoin"
        Kind.ONCHAIN -> "On Bitcoin"
        Kind.LIGHTNING -> "Paid over Lightning"
        Kind.INVOICE -> "Received over Lightning"
        Kind.PAYOUT -> "Sent on Bitcoin"
    }
}

/** Whether it paid out, rather than in. */
private fun paid(activity: Activity): Boolean =
    activity.kind == Kind.SENT || activity.kind == Kind.LIGHTNING || activity.kind == Kind.PAYOUT

/** Where it stands, in words. */
private fun status(activity: Activity, coming: Boolean, details: Details?): String = when {
    coming -> "Waiting for a block, about 10 minutes"
    activity.state == State.FAILED && details is Details.Payout ->
        "${activity.detail}. What it locked comes back once the lock expires."
    activity.state == State.FAILED -> activity.detail
    else -> when (details) {
        is Details.Payout -> when {
            activity.state == State.DONE -> "Paid on Bitcoin"
            details.claim != null -> "Sent on Bitcoin: confirming, about 10 minutes"
            details.coin != null -> "The provider paid it on Bitcoin: confirming, about 10 minutes"
            else -> "Waiting for the provider to pay it on Bitcoin"
        }
        is Details.Moved -> moving(activity, details)
        is Details.Coin -> when {
            details.moving != null -> "Moving into Licash"
            else -> "On Bitcoin, moved into Licash once enough"
        }
        is Details.Invoice -> when (activity.state) {
            State.DONE -> "In your Licash"
            else -> "Paid: taking its notes from the hub"
        }
        else -> when {
            activity.state == State.PENDING -> "Under way"
            paid(activity) -> "Paid"
            else -> "Received"
        }
    }
}

/** Where a move stands: the confirmations its hub waits for, first. */
private fun moving(activity: Activity, move: Details.Moved): String {
    if (activity.state == State.DONE) return "In your Licash"
    val paid = move.payment ?: return if (move.pays == null) "Waiting for the hub" else "Waiting for its payment"
    val depth = move.depth ?: 1u
    val left = depth.toInt() - paid.confirmations.toInt()
    return when {
        paid.confirmations == 0u -> "Waiting for a block: $depth needed, about ${depth.toInt() * 10} minutes"
        left > 0 -> "Confirming: ${paid.confirmations} of $depth blocks, about ${left * 10} minutes left"
        else -> "Confirmed: the hub is sending your Licash"
    }
}

/** What it moved, and what it cost. */
@Composable
private fun Summary(activity: Activity, details: Details?, tx: Onchain?) {
    Group("Summary") {
        when (details) {
            is Details.Moved -> {
                Line("Into Licash", sat(activity.amount))
                Line("Arrived", "On Bitcoin, into Licash at ${host(details.hub)}")
                Line("Hub fee", sat(activity.fee))
                val (mining, pays) = details.payment?.fee to details.pays
                mining?.let { Line("Network fee", sat(it)) }
                if (mining != null && pays != null) Line("Paid on Bitcoin", sat(pays + mining))
            }
            is Details.Coin -> {
                Line("Amount", sat(activity.amount))
                Line("Arrived", if (details.change) "As change on Bitcoin" else "On Bitcoin")
            }
            is Details.Lightning -> {
                Line("Invoice", sat(activity.amount))
                Line(if (activity.state == State.FAILED) "Refund fee" else "Fees", sat(activity.fee))
                if (activity.state != State.FAILED) Line("Total", sat(activity.amount + activity.fee))
                Line("Paid", "Over Lightning, through ${host(details.hub)}")
            }
            is Details.Payout -> {
                Line("Paid", sat(activity.amount))
                if (activity.state != State.FAILED) {
                    Line("Fees", sat(activity.fee))
                    Line("Total", sat(activity.amount + activity.fee))
                }
                Line("To", short(details.address))
                Line("From", "Licash at ${host(details.hub)}")
                Line("Through", host(details.provider))
            }
            is Details.Invoice -> {
                Line("Into Licash", sat(activity.amount))
                details.asks?.let {
                    if (it >= activity.amount) Line("Hub fee, the payer's", sat(it - activity.amount))
                    Line("Paid by the payer", sat(it))
                }
                Line("Arrived", "Over Lightning, into Licash at ${host(details.hub)}")
            }
            else -> if (activity.kind == Kind.SENT) {
                Line("Paid", sat(activity.amount))
                Line("Hub fee", sat(activity.fee))
                Line("Total", sat(activity.amount + activity.fee))
                (details as? Details.Notes)?.let { Line("From", "Licash at ${host(it.hub)}") }
            } else {
                Line("Received", sat(activity.amount))
                (details as? Details.Notes)?.let { Line("Arrived", "In Licash at ${host(it.hub)}") }
            }
        }
        tx?.let { Line("Confirmations", confirmations(it, details)) }
        activity.at?.let { Line("Date", at(it)) }
    }
}

/** A long address, its ends shown. */
private fun short(address: String): String =
    if (address.length <= 20) address else address.take(10) + "…" + address.takeLast(8)

private fun confirmations(tx: Onchain, details: Details?): String {
    val depth = (details as? Details.Moved)?.depth
    return when {
        depth != null && tx.confirmations < depth -> "${tx.confirmations} of $depth"
        else -> "${tx.confirmations}"
    }
}

@Composable
private fun Line(label: String, value: String) {
    Row(Modifier.fillMaxWidth().padding(vertical = 12.dp)) {
        Text(label, Modifier.weight(1f), style = MaterialTheme.typography.bodyLarge)
        Spacer(Modifier.width(12.dp))
        Text(
            value,
            style = MaterialTheme.typography.bodyLarge,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.End,
        )
    }
}

/** Everything known of it, in advanced mode: a tap copies a value. */
@Composable
private fun Technical(activity: Activity, details: Details?, copy: (String, String) -> Unit) {
    Group("Advanced") {
        when (details) {
            is Details.Notes -> {
                Fact("Payment", activity.id, copy)
                Fact("Hub", details.hub, copy)
            }
            is Details.Moved -> {
                Fact("Deposit hash", details.hash, copy)
                Fact("Hub", details.hub, copy)
                details.address?.let { Fact("Address", it, copy) }
                details.pays?.let { Fact("Pays", digits(it), copy) }
                details.payment?.let { Transaction(it, copy) }
                details.depth?.let { Fact("Confirmations needed", "$it", copy) }
                details.fundBy?.let { Fact("Confirmed by block", "$it", copy) }
                details.timeout?.let { Fact("Refundable after block", "$it", copy) }
                details.refund?.let { Fact("Refund", it, copy) }
            }
            is Details.Coin -> {
                Transaction(details.tx, copy)
                Fact("Output", "${details.vout}", copy)
                if (details.change) Fact("Change", "of a payment of this wallet's", copy)
                details.moving?.let { Fact("Spent by", it, copy) }
            }
            is Details.Lightning -> {
                Fact("Lock", activity.id, copy)
                Fact("Hub", details.hub, copy)
                Fact("Invoice", details.invoice, copy)
                details.preimage?.let { Fact("Preimage", it, copy) }
                details.failure?.let { Fact("Failure", it, copy) }
            }
            is Details.Payout -> {
                Fact("Payout hash", activity.id, copy)
                Fact("Address", details.address, copy)
                Fact("Hub", details.hub, copy)
                Fact("Provider", details.provider, copy)
                details.coin?.let { Fact("Provider's coin", it, copy) }
                details.claim?.let { Fact("Claim", it, copy) }
                details.failure?.let { Fact("Failure", it, copy) }
            }
            is Details.Invoice -> {
                Fact("Payment hash", activity.id, copy)
                Fact("Hub", details.hub, copy)
                details.invoice?.let { Fact("Invoice", it, copy) }
                details.failure?.let { Fact("Failure", it, copy) }
            }
            null -> Fact("Id", activity.id, copy)
        }
    }
}

@Composable
private fun Transaction(tx: Onchain, copy: (String, String) -> Unit) {
    Fact("Transaction", tx.txid, copy)
    tx.height?.let { Fact("Block", "$it", copy) }
    tx.fee?.let { Fact("Network fee", digits(it), copy) }
}

@Composable
private fun Fact(label: String, value: String, copy: (String, String) -> Unit) {
    Column(Modifier.fillMaxWidth().clickable { copy(label, value) }.padding(vertical = 10.dp)) {
        Text(label, style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
        Text(value, style = MaterialTheme.typography.bodyMedium, fontFamily = FontFamily.Monospace)
    }
}
