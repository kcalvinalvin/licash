package org.licash.wallet.ui

import android.content.Intent
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.Warning
import androidx.compose.material.icons.outlined.Info
import androidx.compose.material3.Icon
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import org.licash.core.InvoiceQuote
import org.licash.core.LicashException
import org.licash.core.PayQuote
import org.licash.core.PayoutQuote
import org.licash.core.Scanned
import org.licash.core.State
import org.licash.wallet.Engine
import org.licash.wallet.Ui
import org.licash.wallet.explain
import org.licash.wallet.feeOf
import org.licash.wallet.host
import org.licash.wallet.sat
import org.licash.wallet.share

private sealed interface Step {
    data object Looking : Step
    /** A code to pay in notes; on Bitcoin at `address` if it cannot be. */
    data class Paying(
        val code: String,
        val asked: ULong?,
        val address: String? = null,
        val visit: Boolean = false,
    ) : Step
    data object Planning : Step
    /** Swapping through `provider`, named `through`, if chosen. */
    data class Review(
        val code: String,
        val amount: ULong?,
        val quote: PayQuote,
        val provider: String? = null,
        val through: String? = null,
    ) : Step
    data class Visit(val code: String, val asked: ULong?, val address: String?) : Step
    data class Unpayable(val why: String, val address: String, val amount: ULong?) : Step
    data class Invoice(val text: String, val quote: InvoiceQuote) : Step
    data class Address(val address: String) : Step
    data class Payout(val quote: PayoutQuote, val asked: ULong? = null) : Step
    data object Busy : Step
    data object Quoting : Step
    data class PayingOut(val amount: ULong, val depth: UInt) : Step
    /** To the code `code`, if one. */
    data class Sent(val amount: ULong, val handover: String?, val code: String? = null) : Step
    data class Paid(val amount: ULong, val fee: ULong) : Step
    data object Later : Step
    data class Received(val amount: ULong) : Step
    data class Trust(val payment: String, val hub: String) : Step
    data class Failed(val why: String, val title: String = "Couldn't pay") : Step
}

/**
 * Scan or paste: a code to pay, or a payment handed over to take; with
 * `cashOut`, a hub, a Bitcoin address to pay all the notes there out to.
 * `ownAddress`, if `link` pays this wallet's own address, is shown so.
 */
@Composable
fun Send(
    engine: Engine,
    ui: Ui,
    link: String?,
    cashOut: String? = null,
    ownAddress: String? = null,
    back: () -> Unit,
) {
    val scope = rememberCoroutineScope()
    val clipboard = LocalClipboard.current
    // Opened with a link: no camera to start.
    var step by remember { mutableStateOf<Step>(if (link == null) Step.Looking else Step.Busy) }
    var empty by remember { mutableStateOf(false) }
    // This wallet's own address, once cashing out to it.
    var own by remember { mutableStateOf(ownAddress) }

    suspend fun taking(payment: String, trust: Boolean): Step =
        engine.receive(payment, trust).fold(
            { Step.Received(it.amount) },
            { e ->
                when (e) {
                    is LicashException.UnknownHub -> Step.Trust(payment, e.url)
                    else -> Step.Failed(explain(e), "Couldn't receive")
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
            { Step.Payout(it, amount) },
            { Step.Failed(explain(it), "Can't send on Bitcoin") },
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
                            else -> Step.Failed("To cash out, scan or paste a Bitcoin address.", "Not a Bitcoin address")
                        }
                        scanned is Scanned.Code -> when {
                            scanned.notes -> Step.Paying(text, scanned.amount)
                            else -> Step.Failed("This code names no hub you use, and no Bitcoin address to pay.", "Can't pay this")
                        }
                        scanned is Scanned.Visit -> Step.Visit(text, scanned.amount, scanned.address)
                        scanned is Scanned.Bitcoin -> when (val asked = scanned.amount) {
                            null -> Step.Address(scanned.address)
                            else -> quoting(scanned.address, asked)
                        }
                        scanned is Scanned.Payment -> taking(text, false)
                        scanned is Scanned.Invoice -> engine.invoiceQuote(text).fold(
                            { Step.Invoice(text, it) },
                            { Step.Failed(explain(it)) },
                        )
                        else -> Step.Failed("Nothing to pay there.", "Nothing to pay")
                    }
                },
                { Step.Failed(explain(it), "Can't read this") },
            )
        }
    }

    suspend fun paying(code: String, amount: ULong?): Step =
        engine.pay(code, amount).fold(
            { Step.Sent(it.amount, it.handover, code) },
            { e -> if (e is LicashException.Pending) Step.Later else Step.Failed(explain(e)) },
        )

    fun pay(code: String, amount: ULong?) {
        step = Step.Busy
        scope.launch { step = paying(code, amount) }
    }

    // Quoted first: paid at once at a hub trusted holding enough, else
    // reviewed, a swap or a hub not added in the way.
    fun plan(s: Step.Paying, amount: ULong?) {
        step = Step.Planning
        scope.launch {
            step = engine.payQuote(s.code, amount).fold(
                { quote ->
                    if (quote.trusted && quote.swapFrom == null) {
                        step = Step.Busy
                        paying(s.code, amount)
                    } else {
                        Step.Review(s.code, amount, quote)
                    }
                },
                { e ->
                    val address = s.address
                    if (address == null || e is LicashException.Insufficient) {
                        Step.Failed(explain(e))
                    } else {
                        Step.Unpayable(explain(e), address, amount ?: s.asked)
                    }
                },
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

    fun payOut(quote: PayoutQuote, asked: ULong?) {
        step = Step.Busy
        scope.launch {
            step = engine.payOut(quote, asked).fold(
                { Step.PayingOut(it.amount, quote.depth) },
                { Step.Failed(explain(it)) },
            )
        }
    }

    LaunchedEffect(link) { link?.let { read(it) } }
    Column(Modifier.fillMaxSize().padding(horizontal = 24.dp)) {
        TopBar(if (cashOut == null && ownAddress == null) "Send" else "Cash out", back)
        Column(Modifier.fillMaxWidth().weight(1f).verticalScroll(rememberScrollState())) {
            when (val s = step) {
                Step.Looking -> {
                    if (cashOut != null) {
                        Spacer(Modifier.height(8.dp))
                        BigButton("To this wallet", Modifier.fillMaxWidth().testTag("to-own")) {
                            scope.launch {
                                engine.bitcoinAddress().fold(
                                    { own = it; quote(it, null) },
                                    { step = Step.Failed(explain(it)) },
                                )
                            }
                        }
                        Text(
                            "Or scan an address",
                            modifier = Modifier.fillMaxWidth().padding(top = 20.dp, bottom = 8.dp),
                            style = MaterialTheme.typography.labelLarge,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            textAlign = TextAlign.Center,
                        )
                    }
                    Scanner(Modifier.fillMaxWidth().aspectRatio(1f).clip(RoundedCornerShape(24.dp))) { read(it) }
                    Spacer(Modifier.height(16.dp))
                    BigButton("Paste", Modifier.fillMaxWidth().testTag("paste"), quiet = true) {
                        scope.launch {
                            val clip = clipboard.getClipEntry()?.clipData
                            val text = clip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.text?.toString()?.trim()
                            empty = text.isNullOrEmpty()
                            if (!text.isNullOrEmpty()) read(text)
                        }
                    }
                    if (empty) {
                        Spacer(Modifier.height(8.dp))
                        Text(
                            "Nothing copied yet",
                            modifier = Modifier.fillMaxWidth(),
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            textAlign = TextAlign.Center,
                        )
                    }
                    // Or someone paid before, by name.
                    if (cashOut == null) Contacts(engine) { read(it.code) }
                }
                is Step.Paying -> Paying(ui, s) { amount -> plan(s, amount) }
                Step.Busy -> Middle("One moment…")
                Step.Planning -> Middle("One moment…")
                is Step.Review -> Review(s.quote, s.through.takeIf { ui.developer }, { pay(s.code, s.amount) }) {
                    // A swap first: any provider that could, to choose.
                    if (ui.developer && s.quote.swapFrom != null) {
                        Providers {
                            engine.payOffers(s.code, s.amount).map { offers ->
                                val swaps = offers.filter { it.provider != null }
                                val current = s.provider ?: swaps.firstOrNull()?.provider
                                swaps.map { o ->
                                    val provider = o.provider!!
                                    val name = o.providerName ?: host(provider)
                                    Offer(
                                        name,
                                        "Fees ${feeOf(o.quote.fee, o.quote.amount)}" + if (o.known) ", used before" else "",
                                        provider == current,
                                    ) {
                                        step = Step.Planning
                                        scope.launch {
                                            step = engine.choosePayOffer(s.code, s.amount, provider).fold(
                                                { Step.Review(s.code, s.amount, it, provider, name) },
                                                { Step.Failed(explain(it)) },
                                            )
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                is Step.Visit -> Ask(
                    "Pay at their hub?",
                    "Their hub is not one of yours.",
                    "Pay there",
                    {
                        val paying = Step.Paying(s.code, s.asked, s.address, visit = true)
                        if (s.asked != null) plan(paying, null) else step = paying
                    },
                    back,
                )
                is Step.Unpayable -> Ask(
                    "Pay on Bitcoin instead?",
                    s.why,
                    "Pay on Bitcoin",
                    {
                        when (val amount = s.amount) {
                            null -> step = Step.Address(s.address)
                            else -> quote(s.address, amount)
                        }
                    },
                    back,
                )
                Step.Quoting -> Middle("Getting a price…")
                is Step.Invoice -> Invoice(s.quote, ui.hubs.size > 1) { payInvoice(s.text, s.quote.hub) }
                is Step.Address -> ToAddress(s.address) { amount -> quote(s.address, amount) }
                is Step.Payout -> Payout(s.quote, s.quote.address == own, ui.hubs.size > 1, { payOut(s.quote, s.asked) }) {
                    if (ui.developer) {
                        Providers {
                            engine.payoutOffers(s.quote.address, s.asked, s.quote.hub).map { offers ->
                                offers.map { o ->
                                    Offer(
                                        o.providerName ?: host(o.provider),
                                        "Fees ${feeOf(o.fee, o.amount)}, pays ${sat(o.amount)}",
                                        o.provider == s.quote.provider,
                                    ) { step = Step.Payout(o, s.asked) }
                                }
                            }
                        }
                    }
                }
                is Step.PayingOut -> Told("Sending ${sat(s.amount)}", "Arrives in ${arrives(s.depth)}", back)
                is Step.Paid -> Done("Paid ${sat(s.amount)}", back, if (s.fee > 0uL) "Fee ${feeOf(s.fee, s.amount)}" else null)
                is Step.Sent -> {
                    Sent(s, back)
                    s.code?.let { SaveContact(engine, it) }
                }
                // Not to pay again: it finishes by itself.
                Step.Later -> Told("Under way", "It finishes by itself", back)
                is Step.Received -> Done("Received ${sat(s.amount)}", back)
                is Step.Trust -> Ask(
                    "Take it?",
                    "It is at ${name(s.hub)}, a hub you don't use yet. Taking it adds that hub.",
                    "Take it",
                    { take(s.payment, true) },
                    back,
                )
                is Step.Failed -> Ask(s.title, s.why, "Try again", { step = Step.Looking }, back, failed = true)
            }
        }
    }
}

/** One fact of a payment to confirm: `tag` names its value, for the tests; a fee's `share` of the amount. */
private class Fact(val label: String, val value: String, val tag: String, val share: String? = null)

/** What a payment costs and when it arrives, to confirm it by: with what it comes to, if it costs. */
private fun costs(amount: ULong, fee: ULong, arrives: String): List<Fact> = listOfNotNull(
    Fact("Fee", if (fee == 0uL) "None" else sat(fee), "fee", share(fee, amount)),
    if (fee > 0uL) Fact("Total", sat(amount + fee), "total") else null,
    Fact("Arrives", arrives, "arrives"),
)

/** How long a payment on Bitcoin takes to count, `depth` blocks deep. */
internal fun arrives(depth: UInt): String = "about ${(depth.toInt() + 1) * 10} min"

/**
 * A payment to confirm: how much, then what there is to know before
 * agreeing, a fact a line, and one button. `head` goes under the amount,
 * `more` under the button.
 */
@Composable
private fun Confirm(
    amount: ULong,
    facts: List<Fact>,
    action: String,
    act: () -> Unit,
    head: @Composable () -> Unit = {},
    more: @Composable () -> Unit = {},
) {
    Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.CenterHorizontally) {
        Spacer(Modifier.height(32.dp))
        Text(sat(amount), modifier = Modifier.testTag("paying"), style = Heading)
        head()
        Facts(facts)
        Spacer(Modifier.height(24.dp))
        BigButton(action, Modifier.fillMaxWidth().testTag("confirm"), onClick = act)
        more()
    }
}

@Composable
private fun Facts(facts: List<Fact>) {
    Group("") {
        facts.forEachIndexed { i, fact ->
            if (i > 0) Divider()
            Line(fact.label, fact.value, tag = fact.tag, share = fact.share)
        }
    }
}

/**
 * A code to pay in notes: the amount it asks, or one typed straight in.
 * The keyboard's action only hides it: the button pays. At a hub not
 * added, what it costs is told next, once known.
 */
@Composable
private fun Paying(ui: Ui, s: Step.Paying, pay: (ULong?) -> Unit) {
    var amount by remember { mutableStateOf(s.asked?.toLong()) }
    // The most a hub would charge: the code may name several.
    val fee = amount?.takeIf { !s.visit }
        ?.let { a -> ui.hubs.maxOfOrNull { it.feeBase + it.feePpm * a.toULong() / 1_000_000uL } }
    Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.CenterHorizontally) {
        Spacer(Modifier.height(32.dp))
        if (s.asked == null) {
            AmountField(amount, { amount = it })
        } else {
            Text(sat(s.asked), modifier = Modifier.testTag("paying"), style = Heading)
        }
        if (!s.visit) {
            Facts(
                listOf(
                    Fact(
                        "Fee",
                        fee?.let { if (it == 0uL) "None" else "Up to ${sat(it)}" } ?: "",
                        "fee",
                        fee?.let { f -> amount?.let { share(f, it.toULong()) } },
                    ),
                    Fact("Arrives", "Instantly", "arrives"),
                ),
            )
        }
        Spacer(Modifier.height(24.dp))
        BigButton(if (s.visit) "Continue" else "Pay", Modifier.fillMaxWidth().testTag("confirm"), enabled = amount != null) {
            pay(if (s.asked == null) amount?.toULong() else null)
        }
    }
}

/**
 * A payment in notes to review: what it pays and costs, the swap bringing
 * notes to its hub first, and whether that hub is one not added.
 */
@Composable
private fun Review(quote: PayQuote, through: String?, pay: () -> Unit, more: @Composable () -> Unit) {
    val swap = quote.swapFrom
    Confirm(
        quote.amount,
        costs(quote.amount, quote.fee, if (swap == null) "Instantly" else "Shortly") + listOfNotNull(
            swap?.let { Fact("Moved first", "${sat(quote.swapped)} from ${name(it)}", "moves") },
            through?.let { Fact("Through", it, "through") },
        ),
        "Pay",
        pay,
        head = {
            if (!quote.trusted) {
                Text(
                    "At ${name(quote.hub)}, a hub you have not added",
                    modifier = Modifier.padding(top = 8.dp).testTag("not-added"),
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    textAlign = TextAlign.Center,
                )
            }
        },
        more = more,
    )
}

/** A provider's offer, in a list to choose from: `choose` takes it. */
private class Offer(val name: String, val detail: String, val current: Boolean, val choose: () -> Unit)

/**
 * Every provider's offer, once asked, `load`ing them: to choose another
 * than the one picked, in advanced mode.
 */
@Composable
private fun Providers(load: suspend () -> Result<List<Offer>>) {
    val scope = rememberCoroutineScope()
    var offers by remember { mutableStateOf<List<Offer>?>(null) }
    var asking by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    Spacer(Modifier.height(16.dp))
    val list = offers
    if (list == null) {
        TextButton(
            enabled = !asking,
            onClick = {
                asking = true
                error = null
                scope.launch {
                    load().fold({ offers = it }, { error = explain(it) })
                    asking = false
                }
            },
            modifier = Modifier.testTag("providers"),
        ) { Text(if (asking) "Asking the providers…" else "All providers") }
    } else {
        Text(
            if (list.size == 1) "The only provider" else "${list.size} providers",
            modifier = Modifier.fillMaxWidth().padding(top = 8.dp),
            style = MaterialTheme.typography.titleSmall,
        )
        list.forEach { o ->
            Row(Modifier.fillMaxWidth().padding(vertical = 6.dp), verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f)) {
                    Text(o.name, style = MaterialTheme.typography.bodyLarge, maxLines = 1, overflow = TextOverflow.Ellipsis)
                    Text(
                        o.detail,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                if (o.current) {
                    Text("Chosen", style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.primary)
                } else {
                    TextButton(onClick = o.choose) { Text("Use") }
                }
            }
        }
    }
    error?.let {
        Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error)
    }
}

/** A Lightning invoice to pay: what it asks, and all it costs on top; from which hub, with `several`. */
@Composable
private fun Invoice(quote: InvoiceQuote, several: Boolean, pay: () -> Unit) {
    Confirm(
        quote.amount,
        costs(quote.amount, quote.fee, "In seconds") + listOfNotNull(
            if (several) Fact("From", name(quote.hub), "from") else null,
        ),
        "Pay",
        pay,
        head = {
            quote.description?.let {
                Text(
                    it,
                    modifier = Modifier.padding(top = 8.dp),
                    style = MaterialTheme.typography.bodyLarge,
                    textAlign = TextAlign.Center,
                    maxLines = 3,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        },
    )
}

/**
 * A Bitcoin address to pay: how much, typed straight in, or all. The
 * keyboard's action continues to what it costs, to confirm.
 */
@Composable
private fun ToAddress(address: String, quote: (ULong?) -> Unit) {
    var amount by remember { mutableStateOf<Long?>(null) }
    Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.CenterHorizontally) {
        Spacer(Modifier.height(32.dp))
        AmountField(amount, { amount = it }, action = ImeAction.Go) { amount?.let { quote(it.toULong()) } }
        Text(
            "To ${shortAddress(address)}",
            modifier = Modifier.testTag("to"),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.height(32.dp))
        BigButton("Continue", Modifier.fillMaxWidth(), enabled = amount != null) {
            amount?.let { quote(it.toULong()) }
        }
        Spacer(Modifier.height(12.dp))
        BigButton("Send all", Modifier.fillMaxWidth(), quiet = true) { quote(null) }
    }
}

/**
 * A payout on Bitcoin to confirm: what the address gets, all it costs on
 * top and when it arrives; from which hub, with `several`.
 */
@Composable
private fun Payout(quote: PayoutQuote, own: Boolean, several: Boolean, pay: () -> Unit, more: @Composable () -> Unit) {
    Confirm(
        quote.amount,
        listOf(Fact("To", if (own) "This wallet" else shortAddress(quote.address), "to")) +
            costs(quote.amount, quote.fee, arrives(quote.depth).replaceFirstChar { it.uppercase() }) +
            listOfNotNull(if (several) Fact("From", name(quote.hub), "from") else null),
        "Send",
        pay,
        more = more,
    )
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
            "Hand them this to finish",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
        )
        Spacer(Modifier.height(16.dp))
        if (s.handover.length <= 1500) QrCode(s.handover)
        Spacer(Modifier.height(16.dp))
        BigButton("Share", Modifier.fillMaxWidth()) {
            val send = Intent(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_TEXT, s.handover)
            context.startActivity(Intent.createChooser(send, null))
        }
        Spacer(Modifier.height(12.dp))
        BigButton("Done", Modifier.fillMaxWidth().testTag("done"), quiet = true, onClick = back)
    }
}

/** What [Outcome] says in its glyph: done, under way, a question, or failed. */
private enum class Tone { DONE, BUSY, ASK, FAILED }

/**
 * Where paying ends, or stops to ask, all alike: a glyph in a disc, a
 * title, a line under it, and its buttons. `titleTag` and `detailTag`
 * name them, for the tests.
 */
@Composable
private fun Outcome(
    tone: Tone,
    title: String,
    detail: String? = null,
    titleTag: String? = null,
    detailTag: String? = null,
    buttons: @Composable ColumnScope.() -> Unit = {},
) {
    val colors = MaterialTheme.colorScheme
    val tint = when (tone) {
        Tone.FAILED -> colors.error
        Tone.ASK -> colors.onSurfaceVariant
        else -> colors.primary
    }
    Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.CenterHorizontally) {
        Spacer(Modifier.height(72.dp))
        Box(
            Modifier.size(88.dp).clip(CircleShape).background(tint.copy(alpha = 0.14f)),
            contentAlignment = Alignment.Center,
        ) {
            when (tone) {
                Tone.BUSY -> CircularProgressIndicator(Modifier.size(40.dp), color = tint, strokeWidth = 3.dp)
                Tone.DONE -> Icon(Icons.Filled.Check, contentDescription = null, Modifier.size(44.dp), tint = tint)
                Tone.ASK -> Icon(Icons.Outlined.Info, contentDescription = null, Modifier.size(44.dp), tint = tint)
                Tone.FAILED -> Icon(Icons.Filled.Warning, contentDescription = null, Modifier.size(40.dp), tint = tint)
            }
        }
        Spacer(Modifier.height(24.dp))
        Text(
            title,
            modifier = titleTag?.let { Modifier.testTag(it) } ?: Modifier,
            style = MaterialTheme.typography.headlineSmall,
            textAlign = TextAlign.Center,
        )
        detail?.let {
            Spacer(Modifier.height(8.dp))
            Text(
                it,
                modifier = detailTag?.let { tag -> Modifier.testTag(tag) } ?: Modifier,
                style = MaterialTheme.typography.bodyLarge,
                color = colors.onSurfaceVariant,
                textAlign = TextAlign.Center,
            )
        }
        Spacer(Modifier.height(40.dp))
        buttons()
    }
}

/** Waiting on something quick: what, under a spinner. */
@Composable
private fun Middle(text: String) = Outcome(Tone.BUSY, text)

/** It went through: said in a few words, `sub` under them if any. */
@Composable
private fun Done(text: String, back: () -> Unit, sub: String? = null) = Outcome(Tone.DONE, text, sub, titleTag = "result") {
    BigButton("Done", Modifier.fillMaxWidth().testTag("done"), onClick = back)
}

/** It is under way: what, and what to expect of it. */
@Composable
private fun Told(text: String, sub: String, back: () -> Unit) = Outcome(Tone.BUSY, text, sub, titleTag = "result") {
    BigButton("Done", Modifier.fillMaxWidth().testTag("done"), onClick = back)
}

/**
 * A question, or what went wrong `failed`: its `title`, why under it
 * (tagged `ask`), and what to do about it or leave.
 */
@Composable
private fun Ask(
    title: String,
    why: String,
    action: String,
    act: () -> Unit,
    back: () -> Unit,
    failed: Boolean = false,
) = Outcome(if (failed) Tone.FAILED else Tone.ASK, title, why, detailTag = "ask") {
    BigButton(action, Modifier.fillMaxWidth().testTag("confirm"), onClick = act)
    Spacer(Modifier.height(12.dp))
    BigButton("Cancel", Modifier.fillMaxWidth().testTag("cancel"), quiet = true, onClick = back)
}
