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
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import org.licash.core.Code
import org.licash.core.DonationAddress
import org.licash.core.LightningInvoice
import org.licash.core.State
import org.licash.wallet.Engine
import org.licash.wallet.explain
import org.licash.wallet.percent
import org.licash.wallet.sat

/** How often an invoice shown is followed, in ms. */
private const val FOLLOW_MS = 3_000L

/** How long copying says it copied, in ms. */
internal const val COPIED_MS = 2_000L

/**
 * What Receive shows: the way it picks for the amount, or one of the
 * others asked for, `label` naming it and `says` who pays it.
 */
internal enum class Way(val label: String, val says: String, val tag: String) {
    /** A Lightning invoice a hub makes, for an amount: any wallet pays it. */
    LIGHTNING("Lightning invoice", "Any Lightning wallet, for an amount", "way-lightning"),

    /** The wallet's code: in Licash, or at its silent payment address. */
    CODE("Licash code", "Licash wallets, cheaper and more private", "way-code"),

    /** The code with an address on Bitcoin any wallet pays, handed out once asked for. */
    BITCOIN("Bitcoin address", "Any Bitcoin wallet, on Bitcoin", "way-bitcoin"),

    /** The silent payment address alone, for wallets paying those only. */
    SILENT("Silent payment address", "Wallets paying silent payments, on Bitcoin", "way-silent"),

    /** A static LNURL of a swap provider's: any wallet pays it, again, while the phone is off. */
    DONATIONS("Donation address", "Any Lightning wallet, again and again, even while you're offline", "way-donations"),
}

/**
 * The way Receive picks for `amount`: an invoice any wallet pays, if a
 * hub trusted takes `lightning` payments; else the wallet's code.
 */
internal fun picked(amount: Long?, lightning: Boolean): Way =
    if (amount != null && lightning) Way.LIGHTNING else Way.CODE

/**
 * The ways to be paid while a hub trusted takes `lightning` payments, and
 * `silent` ones, and while a provider takes `donations` to the wallet.
 */
internal fun ways(lightning: Boolean, silent: Boolean, donations: Boolean = false): List<Way> = listOfNotNull(
    Way.LIGHTNING.takeIf { lightning },
    Way.CODE,
    Way.BITCOIN,
    Way.SILENT.takeIf { silent },
    Way.DONATIONS.takeIf { donations },
)

/** What a donation address takes and costs, and who pays it on, in words. */
internal fun donationWords(address: DonationAddress): String {
    val by = address.name ?: address.provider.substringAfter("://").trimEnd('/')
    val fee = listOfNotNull(
        sat(address.feeBase).takeIf { address.feeBase > 0u },
        percent(address.feePpm).takeIf { address.feePpm > 0u },
    ).joinToString(" + ")
    val paid = when {
        fee.isEmpty() -> "Paid to you in notes by $by, with no fee."
        else -> "Paid to you in notes by $by, less its fee of $fee."
    }
    return when {
        address.stocked -> paid
        else -> "$paid It can't take donations right now: payers are refused until it can."
    }
}

/**
 * What the payer of an invoice asked for an amount pays, and what the
 * wallet gets of it: in words, those differing from the amount only.
 */
internal fun invoiceWords(amount: ULong, asks: ULong, gets: ULong): String? = listOfNotNull(
    "The payer pays ${sat(asks)}".takeIf { asks != amount },
    "You get ${sat(gets)}, the hub's fee taken".takeIf { gets != asks },
).joinToString("\n").ifEmpty { null }

/**
 * What to be paid at, as a QR code and one Copy, picked for the payer: for
 * an amount, a Lightning invoice any wallet pays, from a hub taking
 * payments over Lightning; for any amount, the wallet's code, paid in
 * Licash or at its silent payment address. The other ways, an address on
 * Bitcoin any wallet pays among them, one tap away in a sheet. Under it,
 * what it is and the amount asked, and nothing else.
 */
@Composable
fun Receive(engine: Engine, back: () -> Unit) {
    val clipboard = LocalClipboard.current
    val scope = rememberCoroutineScope()
    var amount by rememberSaveable { mutableStateOf<Long?>(null) }
    var code by remember { mutableStateOf<Code?>(null) }
    var bitcoin by remember { mutableStateOf<Code?>(null) }
    // Looked for once, out of sight: a way to be paid only if found.
    var donations by remember { mutableStateOf<DonationAddress?>(null) }
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
    var choosing by rememberSaveable { mutableStateOf(false) }
    val ui by engine.ui.collectAsState()
    val takesLightning = ui.hubs.any { it.lightning != null }
    val ways = ways(takesLightning, ui.hubs.any { it.silent }, donations != null)
    // Picked for the amount, unless another way was asked for.
    var chosen by rememberSaveable { mutableStateOf<Way?>(null) }
    val way = chosen?.takeIf { it in ways } ?: picked(amount, takesLightning)
    val lightning = way == Way.LIGHTNING
    // An invoice, made by a hub: for an amount, paid once.
    var invoice by remember { mutableStateOf<LightningInvoice?>(null) }
    val request = invoice?.request?.takeIf { lightning }
    val silent = code?.silent?.takeIf { way == Way.SILENT }
    // No amount, as a silent payment address.
    val anyAmount = way == Way.SILENT || way == Way.DONATIONS
    val shown = when (way) {
        Way.LIGHTNING -> request?.let { way.label to it }
        Way.SILENT -> silent?.let { way.label to it }
        Way.DONATIONS -> donations?.let { way.label to it.lnurl }
        Way.CODE -> code?.let { way.label to it.text }
        Way.BITCOIN -> bitcoin?.let { way.label to it.text }
    }
    LaunchedEffect(amount, received) {
        code = null
        error = null
        engine.code(amount?.toULong())
            .onSuccess { code = it }
            .onFailure { error = explain(it) }
    }
    // Only once asked for: an address handed out is watched from then on,
    // and made again once paid.
    LaunchedEffect(way == Way.BITCOIN, amount, received) {
        bitcoin = null
        if (way != Way.BITCOIN) return@LaunchedEffect
        error = null
        engine.bitcoinCode(amount?.toULong())
            .onSuccess { bitcoin = it }
            .onFailure { error = explain(it) }
    }
    LaunchedEffect(ui.hubs) {
        donations = engine.donationAddress().getOrNull()
    }
    LaunchedEffect(lightning, amount) {
        invoice = null
        val asked = amount?.takeIf { lightning } ?: return@LaunchedEffect
        error = null
        engine.askInvoiceFor(asked.toULong())
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
    if (choosing) {
        Sheet("Other ways", dismiss = { choosing = false }) {
            ways.filter { it != way }.forEach { other ->
                Entry(other.label, sub = other.says, tag = other.tag) {
                    choosing = false
                    chosen = other
                    error = null
                    // An invoice asks for an amount.
                    if (other == Way.LIGHTNING && amount == null) edit() else editing = false
                }
            }
        }
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
                lightning -> if (amount != null && error == null) Waiting()
                way == Way.DONATIONS -> donations?.let { QrCode(it.lnurl) }
                way == Way.SILENT -> when (silent) {
                    null -> if (error == null) Waiting()
                    else -> {
                        QrCode(silent.uppercase())
                        Spacer(Modifier.height(8.dp))
                        Text(
                            silent,
                            modifier = Modifier.testTag("silent"),
                            style = MaterialTheme.typography.bodySmall,
                            textAlign = TextAlign.Center,
                        )
                    }
                }
                else -> when (val made = if (way == Way.BITCOIN) bitcoin else code) {
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
                    way.label,
                    modifier = Modifier.testTag("way"),
                    style = MaterialTheme.typography.labelLarge,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(4.dp))
                Text(
                    when {
                        way == Way.SILENT -> "Any amount, on Bitcoin"
                        way == Way.DONATIONS -> donations?.let { "${sat(it.min)} to ${sat(it.max)}" } ?: "Any amount"
                        else -> amount?.let { sat(it.toULong()) } ?: if (lightning) "Choose an amount" else "Any amount"
                    },
                    modifier = Modifier.testTag("asking"),
                    style = MaterialTheme.typography.titleMedium,
                )
            }
            // What the payer pays and the wallet gets, and what the hub is waiting for: nothing else.
            val made = invoice?.takeIf { request != null }
            val paying = made?.asks?.let { asks -> amount?.let { invoiceWords(it.toULong(), asks, made.amount) } }
            val waits = invoice?.reason?.takeIf { lightning && amount != null && request == null }
            val says = when (way) {
                Way.SILENT -> "Found once in a block. Each payment lands at an address of its own."
                Way.BITCOIN -> "Found once in a block. Once paid, the next address shows."
                Way.DONATIONS -> donations?.let { donationWords(it) }
                else -> paying ?: waits
            }
            says?.let {
                Text(
                    it,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    textAlign = TextAlign.Center,
                )
            }
            if (!editing) {
                TextButton(onClick = { choosing = true }, modifier = Modifier.testTag("other-ways")) {
                    Text("Other ways")
                }
            }
        }
        Row(Modifier.padding(vertical = 24.dp), horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            if (editing) {
                BigButton("Cancel", Modifier.weight(1f), quiet = true) { editing = false }
                BigButton("Done", Modifier.weight(1f).testTag("done")) { done() }
            } else {
                // A silent payment address or a donation one asks for no amount.
                if (!anyAmount) {
                    BigButton("Amount", Modifier.weight(1f).testTag("set-amount"), quiet = true, onClick = edit)
                }
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
