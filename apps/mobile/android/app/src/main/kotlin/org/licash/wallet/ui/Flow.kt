package org.licash.wallet.ui

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.drawscope.DrawScope
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.TextMeasurer
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.drawText
import androidx.compose.ui.text.rememberTextMeasurer
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Constraints
import androidx.compose.ui.unit.dp
import org.licash.core.Activity
import org.licash.core.Kind
import org.licash.core.NotesOf
import org.licash.core.PaidTo
import org.licash.core.State
import org.licash.wallet.digits
import org.licash.wallet.sat

/*
 * The notes a line of the history spent and made, drawn: each a bar as long
 * as its amount's bits (a note twice as large, a step longer), those spent on
 * the left, those made on the right by whom they went to.
 */

/** Notes made for one part of a payment: `to` whom, or the wallet's (`null`, as received). */
internal data class Bucket(val to: PaidTo?, val amount: ULong, val notes: List<ULong>)

/** The notes spent, and those made, by whom they went to. */
internal data class Flow(val spent: List<ULong>, val out: List<Bucket>)

/** The bits of `amount`: 1 for 1 sat, 13 for 4,096. */
internal fun bits(amount: ULong): Int = 64 - amount.countLeadingZeroBits()

/**
 * What `notes` spent and made, drawn: the parts paid, payee first; or,
 * none paid (as received), the notes made the wallet's. Nothing to draw,
 * `null`.
 */
internal fun flowOf(notes: NotesOf): Flow? {
    val spent = notes.spent.map { it.amount }
    val paid = notes.paid.filter { it.amount > 0uL }.sortedBy { it.to.ordinal }.map { Bucket(it.to, it.amount, it.notes) }
    val out = paid.ifEmpty {
        notes.made.takeIf { it.isNotEmpty() }
            ?.let { made -> listOf(Bucket(null, made.sumOf { it.amount }, made.map { it.amount })) }
            .orEmpty()
    }
    return if (spent.isEmpty() && out.isEmpty()) null else Flow(spent, out)
}

/** Whom a part of a payment of `kind` paid, in words. */
internal fun paidTo(kind: Kind, to: PaidTo?): String = when (to) {
    PaidTo.PAYEE -> when (kind) {
        Kind.LIGHTNING -> "To the invoice, through the hub"
        Kind.PAYOUT, Kind.SWAP -> "To the provider"
        else -> "To the payee"
    }
    PaidTo.ITSELF -> "To this wallet"
    PaidTo.HUB -> "The hub's fee"
    PaidTo.KEPT -> "Kept"
    null -> "Yours"
}

/** Where notes of none spent came from, in words. */
private fun from(kind: Kind): String = when (kind) {
    Kind.MOVED, Kind.FUNDED, Kind.ONCHAIN -> "From Bitcoin"
    Kind.INVOICE -> "Over Lightning"
    Kind.SWAP -> "From the provider"
    else -> "From the payer"
}

/** Whom a part went to, in a word: a heading of the drawing. */
internal fun shortTo(kind: Kind, to: PaidTo?): String = when (to) {
    PaidTo.PAYEE -> when (kind) {
        Kind.LIGHTNING -> "Invoice"
        Kind.PAYOUT, Kind.SWAP -> "Provider"
        else -> "Payee"
    }
    PaidTo.ITSELF -> "This wallet"
    PaidTo.HUB -> "Hub fee"
    PaidTo.KEPT -> "Kept"
    null -> "Yours"
}

private val ROW = 22.dp
private val BAR = 14.dp
private val HEAD = 22.dp
private val LABEL = 46.dp

/** A bar's color by whom it went to; spent, `null`. */
@Composable
private fun colors(): (PaidTo?) -> Color {
    val scheme = MaterialTheme.colorScheme
    return { to ->
        when (to) {
            PaidTo.PAYEE, PaidTo.ITSELF -> scheme.primary
            PaidTo.HUB -> Color(0xFFC98A2B)
            PaidTo.KEPT -> scheme.secondary
            null -> scheme.primary
        }
    }
}

/**
 * The notes `activity` spent and made, drawn: no identifiers, only how
 * large each is and where it went. Under way, the notes not made yet show
 * faint, as one bar for their part.
 */
@Composable
internal fun NotesFlow(activity: Activity, flow: Flow) {
    val measurer = rememberTextMeasurer()
    val colorOf = colors()
    val spentColor = MaterialTheme.colorScheme.onSurfaceVariant
    val small = MaterialTheme.typography.labelSmall
    val pending = activity.state == State.PENDING
    // Each bucket shows its notes, or one bar for its part if none made yet.
    val shown = flow.out.map { b -> b to b.notes.ifEmpty { listOf(b.amount) } }
    val most = (flow.spent + shown.flatMap { it.second }).maxOfOrNull(::bits)?.coerceAtLeast(1) ?: 1
    val leftRows = flow.spent.size.coerceAtLeast(1)
    val rightRows = shown.sumOf { it.second.size }
    val height = maxOf(HEAD + ROW * leftRows, HEAD * shown.size + ROW * rightRows)
    val described = buildString {
        append("Spent ").append(flow.spent.joinToString { digits(it) }.ifEmpty { "none" }).append(". ")
        shown.forEach { (b, notes) ->
            append(shortTo(activity.kind, b.to)).append(": ").append(notes.joinToString { digits(it) }).append(". ")
        }
    }
    Text(
        "Each bar is a note: twice as large, a step longer.",
        modifier = Modifier.padding(top = 8.dp, bottom = 8.dp),
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )
    Canvas(Modifier.fillMaxWidth().height(height).semantics { contentDescription = described }) {
        val w = size.width
        // Spent on the left, made on the right, the lines between.
        val leftEdge = w * 0.36f
        val rightEdge = w * 0.56f
        val longest = minOf(leftEdge, w - rightEdge) - LABEL.toPx()
        fun length(amount: ULong) = longest * bits(amount).coerceAtLeast(1) / most
        val row = ROW.toPx()
        val text = 3.dp.toPx()

        // Spent: bars ending at the column's edge, amounts outside.
        var y = (size.height - (HEAD + ROW * leftRows).toPx()) / 2
        val heading = if (flow.spent.isEmpty()) from(activity.kind) else "Spent · ${sat(flow.spent.sumOf { it })}"
        label(measurer, heading, small.copy(color = spentColor), 0f, y + text, leftEdge)
        y += HEAD.toPx()
        val ends = mutableListOf<Offset>()
        if (flow.spent.isEmpty()) ends += Offset(leftEdge, y + row / 2)
        for (amount in flow.spent) {
            val len = length(amount)
            bar(Offset(leftEdge - len, y + (row - BAR.toPx()) / 2), len, spentColor, 0.8f)
            label(measurer, digits(amount), small.copy(color = spentColor), 0f, y + text, leftEdge - len - 4.dp.toPx(), end = true)
            ends += Offset(leftEdge, y + row / 2)
            y += row
        }
        val middle = Offset((leftEdge + rightEdge) / 2, size.height / 2)
        for (end in ends) curve(end, middle, spentColor.copy(alpha = 0.45f))

        // Made, by whom: a heading, then bars from the column's edge, amounts outside.
        y = (size.height - (HEAD * shown.size + ROW * rightRows).toPx()) / 2
        for ((bucket, notes) in shown) {
            val color = colorOf(bucket.to)
            val title = "${shortTo(activity.kind, bucket.to)} · ${sat(bucket.amount)}"
            label(measurer, title, small.copy(color = color), rightEdge, y + text, w - rightEdge)
            y += HEAD.toPx()
            val faint = pending && bucket.notes.isEmpty()
            for (amount in notes) {
                val len = length(amount)
                curve(middle, Offset(rightEdge, y + row / 2), color.copy(alpha = if (faint) 0.25f else 0.55f))
                bar(Offset(rightEdge, y + (row - BAR.toPx()) / 2), len, color, if (faint) 0.35f else 1f)
                label(measurer, digits(amount), small.copy(color = color), rightEdge + len + 4.dp.toPx(), y + text, w - rightEdge - len - 4.dp.toPx())
                y += row
            }
        }
        drawCircle(spentColor, radius = 3.dp.toPx(), center = middle)
    }
}

private fun DrawScope.bar(at: Offset, length: Float, color: Color, alpha: Float) {
    drawRoundRect(color, at, Size(length, BAR.toPx()), CornerRadius(4.dp.toPx()), alpha = alpha)
}

/** A line from `a` to `b`, leaving and arriving level. */
private fun DrawScope.curve(a: Offset, b: Offset, color: Color) {
    val mid = (a.x + b.x) / 2
    val path = Path().apply {
        moveTo(a.x, a.y)
        cubicTo(mid, a.y, mid, b.y, b.x, b.y)
    }
    drawPath(path, color, style = Stroke(width = 1.5.dp.toPx()))
}

/** `text` on one line at most `width` wide from `x`, or ending at `x + width` if `end`. */
private fun DrawScope.label(
    measurer: TextMeasurer,
    text: String,
    style: TextStyle,
    x: Float,
    y: Float,
    width: Float,
    end: Boolean = false,
) {
    if (width <= 0f) return
    val laid = measurer.measure(
        text,
        style,
        overflow = TextOverflow.Ellipsis,
        maxLines = 1,
        constraints = Constraints(maxWidth = width.toInt()),
    )
    val left = if (end) x + width - laid.size.width else x
    drawText(laid, topLeft = Offset(left, y))
}
