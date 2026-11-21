package org.licash.wallet.ui

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.ArrowForward
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.graphics.vector.addPathNodes
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import java.time.DayOfWeek
import java.time.temporal.WeekFields
import java.util.Locale
import org.licash.core.BalancePoint
import org.licash.core.BalanceSample
import org.licash.core.Stats
import org.licash.core.Way
import org.licash.wallet.Engine
import org.licash.wallet.Ui
import org.licash.wallet.digits
import org.licash.wallet.at
import org.licash.wallet.explain
import org.licash.wallet.sat
import org.licash.wallet.share

/**
 * Stats: what came in and went out in a day, week, month, year or all of
 * it, what it cost and how long it took, so that what is cheap and what is
 * dear shows. Read from the wallet's own records when opened and as its
 * history changes; the balance is replayed from them.
 */
@Composable
fun StatsScreen(engine: Engine, ui: Ui, back: () -> Unit) {
    val now = remember { System.currentTimeMillis() / 1000 }
    var period by rememberSaveable { mutableStateOf(Period.MONTH) }
    var ago by rememberSaveable { mutableIntStateOf(0) }
    var stats by remember { mutableStateOf<Stats?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    var points by remember { mutableStateOf<List<BalancePoint>>(emptyList()) }
    // None written down yet is `null`; one that cannot be read, `failed`.
    var samples by remember { mutableStateOf<List<BalanceSample>?>(null) }
    var unread by remember { mutableStateOf(false) }
    val first = stats?.events?.mapNotNull { it.at }?.minOrNull()
    val firstDay = remember { WeekFields.of(Locale.getDefault()).firstDayOfWeek }
    val shown = span(period, now, ago, firstDay = firstDay)
    LaunchedEffect(ui.history) {
        engine.stats().onSuccess { stats = it; error = null }.onFailure { error = explain(it) }
    }
    LaunchedEffect(ui.balance) {
        engine.balanceSamples().onSuccess { samples = it; unread = false }.onFailure { unread = true }
    }
    LaunchedEffect(period, ago, first, ui.history) {
        engine.balanceOver(instants(shown, now, first)).onSuccess { points = it }
    }
    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(horizontal = 20.dp).testTag("stats")) {
        TopBar("Stats", back) {
            Info(
                "Stats",
                "Worked out on your phone from your own payments. Fees count once paid; a payment that failed costs " +
                    "nothing. Usual is the middle of them, so one odd payment does not move it; the average is in the details.",
            )
        }
        Segmented(Period.entries.map { it.word }, period.ordinal, Modifier.fillMaxWidth()) {
            period = Period.entries[it]
            ago = 0
        }
        if (period != Period.ALL) {
            Stepper(
                label(period, now, ago, firstDay = firstDay),
                older = first != null && shown.start > first,
                newer = ago > 0,
                back = { ago++ },
                forward = { ago-- },
            )
        }
        error?.let {
            Text(it, Modifier.padding(4.dp).testTag("error"), MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall)
        }
        val data = stats
        if (data == null) {
            if (error == null) Quiet("Reading your payments")
            return@Column
        }
        val summary = summarize(data.events, data.unlinks, shown)
        if (summary.events == 0 && points.all { it.total == 0uL }) {
            Quiet(if (period == Period.ALL) "Nothing yet" else "Nothing this ${period.word.lowercase()}")
            return@Column
        }
        Overview(summary)
        Balance(points, ui.developer)
        Protected(samples, unread, shown, now, period)
        Flows(summary)
        Costing(summary)
        Spending(summary, ui.developer)
        Timing(summary, ui.developer)
        Spacer(Modifier.height(32.dp))
    }
}

/** The word of a way, as the app says it everywhere. */
private fun Way.said() = when (this) {
    Way.LICASH -> "In Licash"
    Way.LIGHTNING -> "Over Lightning"
    Way.BITCOIN -> "On Bitcoin"
}

@Composable
private fun Quiet(text: String) {
    Text(
        text,
        Modifier.padding(horizontal = 4.dp, vertical = 24.dp).testTag("stats-none"),
        MaterialTheme.colorScheme.onSurfaceVariant,
        style = MaterialTheme.typography.bodyLarge,
    )
}

/** The period asked for, with a step to the one before and the one after. */
@Composable
private fun Stepper(label: String, older: Boolean, newer: Boolean, back: () -> Unit, forward: () -> Unit) {
    Row(Modifier.fillMaxWidth().padding(top = 8.dp), verticalAlignment = Alignment.CenterVertically) {
        IconButton(onClick = back, enabled = older, modifier = Modifier.testTag("stats-older")) {
            Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Earlier")
        }
        Text(
            label,
            Modifier.weight(1f).testTag("stats-period"),
            style = MaterialTheme.typography.titleMedium,
            textAlign = androidx.compose.ui.text.style.TextAlign.Center,
        )
        IconButton(onClick = forward, enabled = newer, modifier = Modifier.testTag("stats-newer")) {
            Icon(Icons.AutoMirrored.Filled.ArrowForward, contentDescription = "Later")
        }
    }
}

/** What the fees came to, first: the number that says whether it is cheap. */
@Composable
private fun Overview(s: Summary) {
    val sent = s.sent.values.fold(0uL) { sum, t -> sum + t.amount }
    Spacer(Modifier.height(16.dp))
    Text("Fees paid", Modifier.padding(horizontal = 4.dp), MaterialTheme.colorScheme.onSurfaceVariant, style = MaterialTheme.typography.labelLarge)
    Row(Modifier.padding(horizontal = 4.dp), verticalAlignment = Alignment.Bottom) {
        Text(
            if (s.fees.total == 0uL) "None" else sat(s.fees.total),
            Modifier.testTag("stats-fees"),
            style = Heading,
        )
        share(s.fees.total, sent)?.let {
            Text(
                " · $it of what you sent",
                Modifier.padding(bottom = 6.dp).testTag("stats-fees-share"),
                MaterialTheme.colorScheme.outline,
                style = MaterialTheme.typography.bodyMedium,
            )
        }
    }
}

/** What the wallet held, drawn over the period, and its start, end and change. */
@Composable
private fun Balance(points: List<BalancePoint>, developer: Boolean) {
    val change = change(points) ?: return
    Group("Balance") {
        Chart(points)
        Line("At the start", sat(change.start), tag = "stats-start")
        Line("At the end", sat(change.end), tag = "stats-end")
        val sign = if (change.up) "+" else "−"
        Line(
            "Change",
            if (change.by == 0uL) "None" else "$sign${sat(change.by)}",
            tag = "stats-change",
            share = share(change.by, change.start),
        )
        val last = points.last()
        More("Where it was") {
            Line("In Licash", sat(last.licash))
            if (last.bitcoin > 0uL) Line("On Bitcoin", sat(last.bitcoin))
            // A hub is named by its host, as everywhere: in basic mode too, as that is its name.
            last.hubs.forEach { Line(org.licash.wallet.ui.name(it.hub), sat(it.amount)) }
            Line("Most", sat(points.maxOf { it.total }))
            Line("Least", sat(points.minOf { it.total }))
            if (developer) Line("Read at", digits(points.size.toULong()) + " moments")
        }
    }
}

/**
 * How much of what was held was protected on average, from what the wallet
 * wrote down as its balance changed: only from the first time this version
 * ran, and only for the time it was written down, which it says.
 */
@Composable
private fun Protected(samples: List<BalanceSample>?, unread: Boolean, shown: Span, now: Long, period: Period) {
    Group("Protected") {
        val a = averaged(samples.orEmpty(), shown, now)
        when {
            unread -> Say("Not available right now.")
            samples.isNullOrEmpty() -> Say("Written down from the day this version first ran: nothing yet.")
            a == null -> Say("Nothing was written down ${if (period == Period.ALL) "yet" else "in this " + period.word.lowercase()}.")
            else -> {
                Line("Protected, on average", sat(a.protected), tag = "stats-protected", share = ppmShare(a.ppm))
                Line("Not protected, on average", sat(a.unprotected), tag = "stats-unprotected", share = ppmShare(1_000_000L - a.ppm))
                if (a.recorded < a.spanSecs) {
                    val pct = a.recorded * 100 / a.spanSecs
                    Line("Written down for", if (pct == 0L) "under 1% of it" else "$pct% of it", tag = "stats-recorded")
                }
                if (a.since > shown.start) Line("Since", at(a.since), tag = "stats-since")
            }
        }
    }
}

/** What came and what went, how many and how much, by way. */
@Composable
private fun Flows(s: Summary) {
    if (s.received.isNotEmpty()) {
        Group("Received") {
            s.received.forEach { (way, t) -> Line(way.said(), tallied(t), tag = "stats-received ${way.name}") }
            if (s.received.size > 1) Line("All", tallied(s.received.values.fold(Tally()) { a, b -> Tally(a.count + b.count, a.amount + b.amount) }))
        }
    }
    if (s.sent.isNotEmpty() || s.failed > 0) {
        Group("Sent") {
            s.sent.forEach { (way, t) -> Line(way.said(), tallied(t), tag = "stats-sent ${way.name}") }
            if (s.sent.size > 1) Line("All", tallied(s.sent.values.fold(Tally()) { a, b -> Tally(a.count + b.count, a.amount + b.amount) }))
            if (s.failed > 0) Line("Couldn't pay", digits(s.failed.toULong()), tag = "stats-failed")
        }
    }
}

/**
 * Each way of paying, the cheapest first: what it usually cost and how
 * long it usually took, its average a tap away.
 */
@Composable
private fun Costing(s: Summary) {
    val ranked = s.sending.entries.sortedBy { it.value.usualPpm ?: Long.MAX_VALUE }
    val others = listOfNotNull(
        s.moving?.let { Triple("From Bitcoin to notes", it, s.moved) },
        s.swapping?.let { Triple("Between your hubs", it, s.moved) },
        s.protecting?.let { Triple("Protection", it, s.covered) },
    )
    if (ranked.isEmpty() && others.isEmpty()) return
    Group("What it costs") {
        val worst = ranked.maxOfOrNull { it.value.usualPpm ?: 0L } ?: 0L
        ranked.forEachIndexed { i, (way, c) ->
            CostRow(way.said(), c, cheapest = i == 0 && ranked.size > 1, bar = bar(c.usualPpm, worst), tag = "cost ${way.name}")
        }
        if (ranked.isNotEmpty() && others.isNotEmpty()) Divider()
        others.forEach { (title, c, _) -> CostRow(title, c, cheapest = false, bar = null, tag = "cost $title") }
    }
}

/** How long a fee's bar is against the dearest's: at least a sliver. */
private fun bar(ppm: Long?, worst: Long): Float? =
    if (ppm == null || worst <= 0) null else (ppm.toFloat() / worst).coerceIn(0.04f, 1f)

@Composable
private fun CostRow(title: String, c: Costs, cheapest: Boolean, bar: Float?, tag: String) {
    var open by remember { mutableStateOf(false) }
    val times = c.usualSecs?.let { " · usually ${took(it)}" } ?: ""
    val sub = (if (cheapest) "Cheapest · " else "") + "${digits(c.count.toULong())} paid" + times
    Column {
        Entry(title, value = cost(c.usualFee, c.usualPpm), sub = sub, tag = tag, onClick = { open = true })
        if (bar != null) {
            Box(
                Modifier.fillMaxWidth(bar).height(4.dp).background(MaterialTheme.colorScheme.primary.copy(alpha = 0.55f), RoundedCornerShape(2.dp)),
            )
            Spacer(Modifier.height(8.dp))
        }
    }
    if (open) {
        Sheet(title, dismiss = { open = false }) {
            Line("Paid", digits(c.count.toULong()))
            Line("Usual fee", cost(c.usualFee, c.usualPpm), tag = "usual-fee")
            Line("Average fee", cost(c.meanFee, c.meanPpm), tag = "mean-fee")
            c.usualSecs?.let { Line("Usual time", took(it)) }
            c.meanSecs?.let { Line("Average time", took(it)) }
        }
    }
}

/** What was paid, for what, and to whom. */
@Composable
private fun Spending(s: Summary, developer: Boolean) {
    val f = s.fees
    if (f.total == 0uL) return
    Group("Fees by purpose") {
        f.sending.forEach { (way, fee) ->
            Line("Sending ${way.said().replaceFirstChar { it.lowercase() }}", sat(fee), share = share(fee, s.sent[way]?.amount ?: 0uL))
        }
        if (f.moving > 0uL) Line("Moving", sat(f.moving), share = share(f.moving, s.moved.amount))
        if (f.upkeep > 0uL) Line("Keeping notes in shape", sat(f.upkeep), share = share(f.upkeep, s.reshaped.amount))
        if (f.protection > 0uL) Line("Protection", sat(f.protection), share = share(f.protection, s.covered.amount))
        More("Who was paid") {
            Line("Hubs", sat(f.hub), tag = "paid-hubs")
            if (f.provider > 0uL) Line("Swap providers", sat(f.provider))
            if (f.network > 0uL) Line("Bitcoin's miners", sat(f.network))
            if (f.unsplit > 0uL) Line("Not itemized", sat(f.unsplit))
        }
        if (developer) {
            val parts = listOfNotNull(
                s.optimizing?.let { "Optimizing" to it },
                s.renewing?.let { "Renewing" to it },
                s.normalizing?.let { "Exact notes" to it },
            )
            if (parts.isNotEmpty()) {
                More("Note upkeep") {
                    parts.forEach { (title, c) ->
                        Line(title, "${digits(c.count.toULong())} · usually ${sat(c.usualFee)}", tag = "upkeep $title", share = ppmShare(c.usualPpm))
                    }
                }
            }
        }
    }
}

/** How long things usually took. */
@Composable
private fun Timing(s: Summary, developer: Boolean) {
    val rows = buildList {
        s.sending.forEach { (way, c) -> c.usualSecs?.let { add("Sending ${way.said().replaceFirstChar { it.lowercase() }}" to it) } }
        s.swapping?.usualSecs?.let { add("Moving between hubs" to it) }
        s.optimizing?.usualSecs?.let { add("Optimizing notes" to it) }
        if (developer) s.renewing?.usualSecs?.let { add("Renewing notes" to it) }
        s.unlinking?.let { (_, times) -> add("Until notes are unlinked" to times.first) }
    }
    if (rows.isEmpty()) return
    Group("How long it takes") {
        rows.forEach { (title, secs) -> Line(title, took(secs), tag = "took $title") }
        More("About these times") {
            Say("The middle of them, from the start of each to its end. A payment that is not done yet, or failed, is not timed.")
            s.unlinking?.let { (count, times) ->
                Line("Notes unlinked", digits(count.toULong()))
                Line("Average wait", took(times.second))
            }
        }
    }
}

/** The balance over time: a line and the area under it, no more than the numbers say. */
@Composable
private fun Chart(points: List<BalancePoint>) {
    val line = MaterialTheme.colorScheme.primary
    val said = points.firstOrNull()?.let { first ->
        "Balance from ${sat(first.total)} to ${sat(points.last().total)}"
    } ?: "No balance"
    Canvas(Modifier.fillMaxWidth().height(120.dp).padding(vertical = 12.dp).semantics { contentDescription = said }.testTag("stats-chart")) {
        if (points.size < 2) return@Canvas
        val values = points.map { it.total.toDouble() }
        val lo = values.min()
        val hi = values.max()
        val pad = 4.dp.toPx()
        val w = size.width
        val h = size.height - 2 * pad
        // A flat balance is a flat line in the middle.
        fun y(v: Double) = if (hi == lo) pad + h / 2 else (pad + h - (v - lo) / (hi - lo) * h).toFloat()
        fun x(i: Int) = w * i / (points.size - 1)
        val path = Path().apply {
            moveTo(x(0), y(values[0]))
            for (i in 1 until values.size) lineTo(x(i), y(values[i]))
        }
        val area = Path().apply {
            addPath(path)
            lineTo(w, size.height)
            lineTo(0f, size.height)
            close()
        }
        drawPath(area, line.copy(alpha = 0.14f))
        drawPath(path, line, style = Stroke(width = 2.dp.toPx(), cap = StrokeCap.Round))
    }
}

/** A bar chart, from Material's path data (Apache 2.0): what opens Stats. */
internal val StatsGlyph: ImageVector by lazy {
    ImageVector.Builder("stats", 24.dp, 24.dp, 24f, 24f)
        .addPath(addPathNodes("M5,9.2h3V19H5zM10.6,5h2.8v14h-2.8zM16.2,13H19v6h-2.8z"), fill = SolidColor(Color.Black))
        .build()
}
