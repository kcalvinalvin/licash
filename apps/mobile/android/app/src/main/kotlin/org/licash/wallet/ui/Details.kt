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
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.ClipEntry
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import org.licash.core.Activity
import org.licash.core.Details
import org.licash.core.Kind
import org.licash.core.Linking
import org.licash.core.LockStep
import org.licash.core.NoteLine
import org.licash.core.NotesOf
import org.licash.core.Onchain
import org.licash.core.PayoutSteps
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
    var unopened by remember { mutableStateOf(false) }
    var notes by remember { mutableStateOf<NotesOf?>(null) }
    LaunchedEffect(activity, ui.scanned) {
        engine.details(activity)
            .onSuccess { details = it; error = null }
            .onFailure { error = explain(it) }
        if (ui.developer) {
            engine.notesOf(activity).onSuccess { notes = it }.onFailure { AppLog.warn("its notes", it) }
        }
    }
    val copy: (String, String) -> Unit = { label, text ->
        scope.launch { clipboard.setClipEntry(ClipEntry(ClipData.newPlainText(label, text))) }
    }
    val tx = when (val d = details) {
        is Details.Moved -> d.payment
        is Details.Coin -> d.tx
        else -> null
    }
    // A payout's claim pays the address: else the provider's coin, as soon
    // as the provider tells of it.
    val txid = tx?.txid ?: (details as? Details.Payout)?.progress?.let { it.claim ?: it.coin }
        ?: (details as? Details.Contract)?.funding
    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(horizontal = 20.dp)) {
        TopBar(title(activity, coming != null), back)
        Spacer(Modifier.height(12.dp))
        val sign = when {
            activity.kind == Kind.SWAP -> ""
            paid(activity) -> "−"
            else -> "+"
        }
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
        (details as? Details.Payout)?.let { payout ->
            PayoutTimeline(activity, payout, engine::explorerLink) { link ->
                unopened = false
                runCatching { context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(link))) }
                    .onFailure {
                        AppLog.warn("opening $link", it)
                        unopened = true
                    }
            }
        }
        Summary(activity, details, tx)
        txid?.let { engine.explorerLink(it) }?.let { link ->
            Spacer(Modifier.height(16.dp))
            OutlinedButton(
                onClick = {
                    unopened = false
                    runCatching { context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(link))) }
                        .onFailure {
                            AppLog.warn("opening $link", it)
                            unopened = true
                        }
                },
                modifier = Modifier.fillMaxWidth().height(52.dp),
            ) { Text("View on a block explorer") }
            if (unopened) {
                Text(
                    "No app here opens web links.",
                    color = MaterialTheme.colorScheme.error,
                    style = MaterialTheme.typography.bodySmall,
                )
            }
        }
        if (ui.developer) {
            Technical(activity, details, copy)
            notes?.takeIf { it.spent.isNotEmpty() || it.made.isNotEmpty() }?.let { NotesGroup(it, copy) }
        }
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
        Kind.CONTRACT -> "Contract opened"
        Kind.SWAP -> "Moved between hubs"
    }
}

/** Whether it paid out, rather than in. */
private fun paid(activity: Activity): Boolean =
    activity.kind == Kind.SENT || activity.kind == Kind.LIGHTNING || activity.kind == Kind.PAYOUT ||
        activity.kind == Kind.CONTRACT

/** Where it stands, in words. */
private fun status(activity: Activity, coming: Boolean, details: Details?): String = when {
    coming -> "Waiting for a block, about 10 minutes"
    activity.state == State.FAILED && details is Details.Payout -> {
        val back = details.progress.lockExpiry
        when (details.progress.lock) {
            LockStep.LOCKED -> "${activity.detail}. Your notes come back after block $back."
            else -> "${activity.detail}. Your notes stay yours."
        }
    }
    activity.state == State.FAILED && details is Details.Swap -> when (details.lock) {
        null -> "${activity.detail}. Your notes stay yours."
        else -> "${activity.detail}. The notes it locked come back once the lock expires."
    }
    activity.state == State.FAILED -> activity.detail
    else -> when (details) {
        is Details.Payout -> payoutStatus(activity, details.progress)
        is Details.Swap -> when {
            activity.state == State.DONE -> "At ${host(details.to)}"
            details.theirs != null -> "The provider locked notes to you at ${host(details.to)}: claiming them"
            details.lock != null ->
                "Your notes are locked for the provider at ${host(details.from)}: " +
                    "waiting for it to lock as much to you at ${host(details.to)}"
            else -> "Locking your notes at ${host(details.from)}"
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
        is Details.Contract -> when (activity.state) {
            State.DONE -> "Opened at ${host(details.hub)}: Balance shows where it stands"
            else -> "Confirming its funding: protecting once in a block, about 10 minutes"
        }
        else -> when {
            activity.state == State.PENDING -> "Under way"
            paid(activity) -> "Paid"
            else -> "Received"
        }
    }
}

/** Where a payout under way stands, in words. */
private fun payoutStatus(activity: Activity, p: PayoutSteps): String {
    val coinHeight = p.coinHeight
    return when {
        activity.state == State.DONE -> "Paid on Bitcoin"
        p.claim != null -> "Sent to the address: confirming, about 10 minutes"
        coinHeight != null && depthOf(coinHeight, p.tip) >= p.depth.toInt() ->
            "The provider's coin confirmed: claiming it"
        coinHeight != null -> "The provider's coin is confirming"
        p.coin != null -> "The provider sent its coin: waiting for a block, about 10 minutes"
        p.lock == LockStep.LOCKED -> "Waiting for the provider to send its coin"
        else -> "Locking your notes at the hub"
    }
}

/** The confirmations of a transaction in block `height`, at `tip`. */
private fun depthOf(height: UInt?, tip: UInt): Int =
    height?.let { if (tip >= it) (tip - it + 1u).toInt() else 0 } ?: 0

/**
 * Each step of a payout: its notes locked for the provider, the provider's
 * coin sent and confirmed, the coin claimed paying the address, and that
 * confirmed; the block to claim by while it is not. A transaction known
 * links to the block explorer.
 */
@Composable
private fun PayoutTimeline(
    activity: Activity,
    payout: Details.Payout,
    link: (String) -> String?,
    open: (String) -> Unit,
) {
    val p = payout.progress
    // Read once: a record's fields are mutable, so not smart cast.
    val coin = p.coin
    val coinHeight = p.coinHeight
    val claim = p.claim
    val claimHeight = p.claimHeight
    val failed = activity.state == State.FAILED
    val done = activity.state == State.DONE
    val view = { txid: String? ->
        txid?.let(link)?.let { url -> "View on a block explorer" to { open(url) } }
    }
    val coinDepth = depthOf(coinHeight, p.tip)
    val coinConfirmed = claim != null || (coinHeight != null && coinDepth >= p.depth.toInt())
    val hub = host(payout.hub)
    Group("Progress") {
        val (lockMark, lockDetail) = when (p.lock) {
            null -> (if (failed) Mark.FAILED else Mark.NOW) to "Locking them at $hub"
            LockStep.REQUESTED -> (if (failed) Mark.FAILED else Mark.NOW) to "Waiting for $hub to certify the lock"
            LockStep.LOCKED -> Mark.DONE to "At $hub. The provider gets them only once you claim its coin; " +
                "else they come back to you after block ${p.lockExpiry}."
            LockStep.PAID -> Mark.DONE to "Paid to the provider at $hub"
            LockStep.REFUNDED -> Mark.FAILED to "Given back to you at $hub"
            LockStep.ABANDONED -> Mark.FAILED to "Never made: your notes stayed yours"
        }
        Step(lockMark, "Notes locked for the provider", lockDetail)
        Step(
            mark = when {
                coin != null -> Mark.DONE
                !failed && p.lock == LockStep.LOCKED -> Mark.NOW
                else -> Mark.NEXT
            },
            title = "The provider locked bitcoin for you",
            detail = coin?.let { id ->
                "Transaction ${short(id)}" + (p.timeout?.let { ", yours to claim before block $it" } ?: "")
            },
            action = view(coin),
        )
        Step(
            mark = when {
                coinConfirmed -> Mark.DONE
                failed || coin == null -> Mark.NEXT
                else -> Mark.NOW
            },
            title = "Its coin confirmed",
            detail = when {
                coinConfirmed -> coinHeight?.let { "In block $it" }
                coinHeight != null -> "$coinDepth of ${p.depth} confirmations"
                coin != null && !failed -> "Waiting for a block, about 10 minutes"
                else -> null
            },
        )
        Step(
            mark = when {
                claim != null -> Mark.DONE
                failed || !coinConfirmed -> Mark.NEXT
                else -> Mark.NOW
            },
            title = "Claimed, paying the address",
            detail = when {
                claim != null -> "Transaction ${short(claim)}, broadcast by this phone"
                coinConfirmed && !failed -> "Claiming it"
                else -> null
            },
            action = view(claim),
        )
        Step(
            mark = when {
                done -> Mark.DONE
                failed || claim == null -> Mark.NEXT
                else -> Mark.NOW
            },
            title = "Payment confirmed",
            detail = when {
                done -> claimHeight?.let { "In block $it" } ?: "Confirmed"
                claimHeight != null -> "${depthOf(claimHeight, p.tip)} of ${p.depth} confirmations"
                claim != null && !failed -> "Waiting for a block, about 10 minutes"
                else -> null
            },
            last = true,
        )
    }
    val by = p.claimBy
    if (!done && !failed && claim == null && by != null) {
        Text(
            deadline(by, p.tip),
            modifier = Modifier.padding(horizontal = 4.dp, vertical = 8.dp),
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

/** The block to claim a payout's coin by, and the time left, roughly. */
private fun deadline(by: UInt, tip: UInt): String {
    val open = "This phone claims it while the app is open."
    return when {
        tip == 0u -> "To be claimed by block $by. $open"
        tip > by -> "Block $by, the last to claim it at, has passed: the payout fails, " +
            "and your notes come back once the lock expires."
        else -> "To be claimed by block $by, in about ${duration((by - tip).toInt() * 10)}. $open"
    }
}

/** `minutes`, roughly, in the largest unit that fits. */
private fun duration(minutes: Int): String = when {
    minutes < 60 -> "$minutes minutes"
    minutes < 120 -> "1 hour"
    minutes < 48 * 60 -> "${minutes / 60} hours"
    else -> "${minutes / (24 * 60)} days"
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
            is Details.Contract -> {
                Line("Cost", sat(activity.amount))
                Line("Paid", "On Bitcoin, from a coin of yours")
                Line("At", host(details.hub))
            }
            is Details.Swap -> {
                Line("Moved", sat(activity.amount))
                if (activity.state != State.FAILED) {
                    Line("Fees", sat(activity.fee))
                    Line("Total", sat(activity.amount + activity.fee))
                }
                Line("From", host(details.from))
                Line("To", host(details.to))
                Line("Through", host(details.provider))
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
                details.pays?.let { Fact("Pays", sat(it), copy, "$it") }
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
                val p = details.progress
                p.coin?.let { Fact("Provider's coin", it, copy) }
                p.coinHeight?.let { Fact("Its block", "$it", copy) }
                p.claim?.let { Fact("Claim", it, copy) }
                p.claimHeight?.let { Fact("The claim's block", "$it", copy) }
                p.lock?.let { Fact("Lock", it.name.lowercase(), copy) }
                p.lockExpiry?.let { Fact("Lock refunded from block", "$it", copy) }
                p.timeout?.let { Fact("Provider takes its coin back from block", "$it", copy) }
                p.claimBy?.let { Fact("Claimed by block", "$it", copy) }
                Fact("Confirmations needed", "${p.depth}", copy)
                details.failure?.let { Fact("Failure", it, copy) }
            }
            is Details.Invoice -> {
                Fact("Payment hash", activity.id, copy)
                Fact("Hub", details.hub, copy)
                details.invoice?.let { Fact("Invoice", it, copy) }
                details.failure?.let { Fact("Failure", it, copy) }
            }
            is Details.Contract -> {
                Fact("Contract", activity.id, copy)
                Fact("Hub", details.hub, copy)
                Fact("Funding", details.funding, copy)
            }
            is Details.Swap -> {
                Fact("Swap hash", activity.id, copy)
                Fact("From", details.from, copy)
                Fact("To", details.to, copy)
                Fact("Provider", details.provider, copy)
                details.lock?.let { Fact("Your lock", it, copy) }
                details.theirs?.let { Fact("The provider's lock", it, copy) }
                details.failure?.let { Fact("Failure", it, copy) }
            }
            null -> Fact("Id", activity.id, copy)
        }
    }
}

/**
 * The notes it spent and those it made the wallet's, in advanced mode:
 * each one's hub, and whether its hub can still tell which payment it came
 * from. A tap copies a note's identifier.
 */
@Composable
private fun NotesGroup(notes: NotesOf, copy: (String, String) -> Unit) {
    Group("Notes") {
        val made = notes.made
        val unlinked = made.count { it.linking == Linking.UNLINKED }
        Text(
            listOfNotNull(
                notes.spent.takeIf { it.isNotEmpty() }?.let { "Spent ${count(it)}." },
                made.takeIf { it.isNotEmpty() }?.let {
                    "Made ${count(it)}" + if (unlinked > 0) ", $unlinked unlinked since." else "."
                },
            ).joinToString(" ") + " An unlinked note was signed again blindly by its hub: " +
                "the hub can't tell which payment it came from when it is spent.",
            modifier = Modifier.padding(vertical = 8.dp),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        if (notes.spent.isNotEmpty()) {
            Text("Spent", Modifier.padding(top = 8.dp), style = MaterialTheme.typography.titleSmall)
            notes.spent.forEach { NoteRow(it, spentHere = true, copy = copy) }
        }
        if (made.isNotEmpty()) {
            Text("Made", Modifier.padding(top = 8.dp), style = MaterialTheme.typography.titleSmall)
            made.forEach { NoteRow(it, spentHere = false, copy = copy) }
        }
    }
}

/** `3 notes, 4,096 sat`. */
private fun count(notes: List<NoteLine>): String {
    val total = notes.fold(0uL) { sum, n -> sum + n.amount }
    val what = if (notes.size == 1) "1 note" else "${notes.size} notes"
    return "$what, ${sat(total)}"
}

/** A note: its amount, hub and linking, and where it went or came from. */
@Composable
private fun NoteRow(note: NoteLine, spentHere: Boolean, copy: (String, String) -> Unit) {
    var done by remember { mutableStateOf(false) }
    LaunchedEffect(done) {
        if (done) {
            delay(COPIED_MS)
            done = false
        }
    }
    val linking = when (note.linking) {
        Linking.UNLINKED -> "unlinked"
        Linking.STAYS_LINKED -> "stays linked: its hub refused to unlink it"
        Linking.LINKED -> "linked, to be unlinked"
    }
    val where = when {
        note.gone -> "spent by a copy of this wallet"
        spentHere && note.madeByItself -> "made for this payment's amounts"
        !spentHere && note.spentBy != null -> "spent since"
        else -> null
    }
    Row(
        Modifier.fillMaxWidth().clickable {
            copy("Note", note.id)
            done = true
        }.padding(vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(sat(note.amount), style = MaterialTheme.typography.bodyLarge)
            Text(
                listOfNotNull(host(note.hub), linking, where).joinToString(" · "),
                style = MaterialTheme.typography.bodySmall,
                color = if (note.linking == Linking.UNLINKED) {
                    MaterialTheme.colorScheme.onSurfaceVariant
                } else {
                    MaterialTheme.colorScheme.tertiary
                },
            )
        }
        Spacer(Modifier.width(12.dp))
        Text(
            if (done) "Copied" else short(note.id),
            style = MaterialTheme.typography.bodySmall,
            fontFamily = FontFamily.Monospace,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
private fun Transaction(tx: Onchain, copy: (String, String) -> Unit) {
    Fact("Transaction", tx.txid, copy)
    tx.height?.let { Fact("Block", "$it", copy) }
    tx.fee?.let { Fact("Network fee", sat(it), copy, "$it") }
}

@Composable
private fun Fact(label: String, value: String, copy: (String, String) -> Unit, copied: String = value) {
    var done by remember { mutableStateOf(false) }
    LaunchedEffect(done) {
        if (done) {
            delay(COPIED_MS)
            done = false
        }
    }
    Column(
        Modifier.fillMaxWidth().clickable {
            copy(label, copied)
            done = true
        }.padding(vertical = 10.dp),
    ) {
        Text(
            if (done) "$label · Copied" else label,
            style = MaterialTheme.typography.labelMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Text(value, style = MaterialTheme.typography.bodyMedium, fontFamily = FontFamily.Monospace)
    }
}
