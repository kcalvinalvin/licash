package org.licash.wallet.ui

import java.time.DayOfWeek
import java.time.Instant
import java.time.LocalDate
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import java.time.temporal.TemporalAdjusters
import java.util.Locale
import org.licash.core.BalancePoint
import org.licash.core.Flow as Purpose
import org.licash.core.StatEvent
import org.licash.core.State
import org.licash.core.Unlink
import org.licash.core.Way
import org.licash.wallet.digits
import org.licash.wallet.sat

/*
 * What the Stats screen says, worked out from the core's events: the
 * period asked for, what came and went in it, what it cost and how long it
 * took. Nothing here draws, and nothing is stored: the same events and the
 * same period give the same figures.
 */

/** How much of the past a page of Stats covers. */
enum class Period(val word: String) {
    DAY("Day"),
    WEEK("Week"),
    MONTH("Month"),
    YEAR("Year"),
    ALL("All"),
}

/** A stretch of time in seconds since 1970: from `start`, up to but not with `end`. */
internal data class Span(val start: Long, val end: Long) {
    operator fun contains(at: Long) = at in start until end
}

/** The whole of time: what All covers. */
internal val EVER = Span(Long.MIN_VALUE, Long.MAX_VALUE)

/**
 * The span `back` periods before the one `now` is in, its days by `zone`'s
 * clock and weeks from `firstDay`. All has none before it.
 */
internal fun span(
    period: Period,
    now: Long,
    back: Int,
    zone: ZoneId = ZoneId.systemDefault(),
    firstDay: DayOfWeek = DayOfWeek.MONDAY,
): Span {
    if (period == Period.ALL) return EVER
    val today = Instant.ofEpochSecond(now).atZone(zone).toLocalDate()
    val (from, to) = when (period) {
        Period.DAY -> today.minusDays(back.toLong()).let { it to it.plusDays(1) }
        Period.WEEK -> today.with(TemporalAdjusters.previousOrSame(firstDay)).minusWeeks(back.toLong())
            .let { it to it.plusWeeks(1) }
        Period.MONTH -> today.withDayOfMonth(1).minusMonths(back.toLong()).let { it to it.plusMonths(1) }
        else -> today.withDayOfYear(1).minusYears(back.toLong()).let { it to it.plusYears(1) }
    }
    val at = { day: LocalDate -> day.atStartOfDay(zone).toEpochSecond() }
    return Span(at(from), at(to))
}

/** What a span of `period`, `back` periods ago, is called: `Today`, `Last week`, `October`, `2025`. */
internal fun label(
    period: Period,
    now: Long,
    back: Int,
    zone: ZoneId = ZoneId.systemDefault(),
    locale: Locale = Locale.getDefault(),
    firstDay: DayOfWeek = DayOfWeek.MONDAY,
): String {
    if (period == Period.ALL) return "All time"
    val span = span(period, now, back, zone, firstDay)
    val first = Instant.ofEpochSecond(span.start).atZone(zone).toLocalDate()
    val last = Instant.ofEpochSecond(span.end - 1).atZone(zone).toLocalDate()
    val year = Instant.ofEpochSecond(now).atZone(zone).year
    val format = { day: LocalDate, pattern: String -> day.format(DateTimeFormatter.ofPattern(pattern, locale)) }
    return when (period) {
        Period.DAY -> when (back) {
            0 -> "Today"
            1 -> "Yesterday"
            else -> format(first, if (first.year == year) "MMM d" else "MMM d, yyyy")
        }
        Period.WEEK -> when (back) {
            0 -> "This week"
            1 -> "Last week"
            else -> if (first.month == last.month) {
                format(first, "MMM d") + " – " + format(last, "d")
            } else {
                format(first, "MMM d") + " – " + format(last, "MMM d")
            }
        }
        Period.MONTH -> when (back) {
            0 -> "This month"
            1 -> "Last month"
            else -> format(first, if (first.year == year) "MMMM" else "MMMM yyyy")
        }
        else -> when (back) {
            0 -> "This year"
            1 -> "Last year"
            else -> first.year.toString()
        }
    }
}

/** How many of a thing, and what they came to. */
internal data class Tally(val count: Int = 0, val amount: ULong = 0uL) {
    operator fun plus(event: StatEvent) = Tally(count + 1, amount + event.amount)
}

/** What a kind of thing cost, as its usual (median) and its average, over `count` done. */
internal data class Costs(
    val count: Int,
    val usualFee: ULong,
    val meanFee: ULong,
    /** Fee over amount, in parts per million, for those with an amount. */
    val usualPpm: Long?,
    val meanPpm: Long?,
    val usualSecs: Long?,
    val meanSecs: Long?,
)

/** What the settled fees paid in a span are, whom they paid and what for. */
internal data class Fees(
    val total: ULong = 0uL,
    /** Sending, by how it was sent. */
    val sending: Map<Way, ULong> = emptyMap(),
    /** Bitcoin turned into notes, and notes moved between hubs. */
    val moving: ULong = 0uL,
    /** Optimizing, renewing and the notes made to pay exact amounts. */
    val upkeep: ULong = 0uL,
    val protection: ULong = 0uL,
    /** Of the total, the hub's, swap providers' and Bitcoin's, as recorded. */
    val hub: ULong = 0uL,
    val provider: ULong = 0uL,
    val network: ULong = 0uL,
    /** The rest: a fee not told apart. */
    val unsplit: ULong = 0uL,
)

/** All of a span's figures. */
internal data class Summary(
    val received: Map<Way, Tally>,
    val sent: Map<Way, Tally>,
    /** Payments that failed, and so cost and moved nothing. */
    val failed: Int,
    val fees: Fees,
    /** What sending cost, by way. */
    val sending: Map<Way, Costs>,
    val moving: Costs?,
    val swapping: Costs?,
    val optimizing: Costs?,
    val renewing: Costs?,
    val normalizing: Costs?,
    val protecting: Costs?,
    /** What the fees were charged on: what moved, what was reshaped, what contracts protect at most. */
    val moved: Tally,
    val reshaped: Tally,
    val covered: Tally,
    /** How long notes waited to be unlinked, in seconds. */
    val unlinking: Pair<Int, Pair<Long, Long>>?,
    /** The events in the span at all. */
    val events: Int,
)

private val WAYS = listOf(Way.LICASH, Way.LIGHTNING, Way.BITCOIN)

/** What keeps the notes in shape: optimizing, renewing, and making notes of the exact amounts a payment needs. */
private val UPKEEP = setOf(Purpose.OPTIMIZED, Purpose.RENEWED, Purpose.NORMALIZED)

/** The figures of `events` and `unlinks` within `span`. */
internal fun summarize(events: List<StatEvent>, unlinks: List<Unlink>, span: Span): Summary {
    val within = events.filter { e -> e.at?.let { it in span } ?: (span == EVER) }
    val done = within.filter { it.state == State.DONE }
    fun tally(flow: Purpose) = WAYS.associateWith { way ->
        done.filter { it.flow == flow && it.way == way }.fold(Tally()) { t, e -> t + e }
    }.filterValues { it.count > 0 }
    val of = { flow: Purpose, way: Way? ->
        costs(done.filter { it.flow == flow && (way == null || it.way == way) })
    }
    val moves = done.filter { it.flow == Purpose.MOVED }
    val waited = unlinks.filter { it.at in span || span == EVER }.map { it.waited.toLong() }
    return Summary(
        received = tally(Purpose.RECEIVED),
        sent = tally(Purpose.SENT),
        failed = within.count { it.flow == Purpose.SENT && it.state == State.FAILED },
        fees = fees(within),
        sending = WAYS.mapNotNull { way -> of(Purpose.SENT, way)?.let { way to it } }.toMap(),
        moving = costs(moves.filter { it.way == Way.BITCOIN }),
        swapping = costs(moves.filter { it.way == Way.LICASH }),
        optimizing = of(Purpose.OPTIMIZED, null),
        renewing = of(Purpose.RENEWED, null),
        normalizing = of(Purpose.NORMALIZED, null),
        protecting = of(Purpose.PROTECTION, null),
        moved = done.filter { it.flow == Purpose.MOVED }.fold(Tally()) { t, e -> t + e },
        reshaped = done.filter { it.flow in UPKEEP }.fold(Tally()) { t, e -> t + e },
        covered = done.filter { it.flow == Purpose.PROTECTION }.fold(Tally()) { t, e -> t + e },
        unlinking = waited.takeIf { it.isNotEmpty() }?.let { it.size to (median(it) to it.sum() / it.size) },
        events = within.size,
    )
}

/** The settled fees of `events`: the core already counts none for what failed or is under way. */
internal fun fees(events: List<StatEvent>): Fees {
    var f = Fees()
    for (e in events) {
        if (e.fee == 0uL) continue
        val told = (e.hubFee ?: 0uL) + (e.providerFee ?: 0uL) + (e.networkFee ?: 0uL)
        f = f.copy(
            total = f.total + e.fee,
            hub = f.hub + (e.hubFee ?: 0uL),
            provider = f.provider + (e.providerFee ?: 0uL),
            network = f.network + (e.networkFee ?: 0uL),
            unsplit = f.unsplit + (if (e.fee > told) e.fee - told else 0uL),
        )
        f = when (e.flow) {
            Purpose.SENT -> f.copy(sending = f.sending + (e.way to ((f.sending[e.way] ?: 0uL) + e.fee)))
            Purpose.MOVED -> f.copy(moving = f.moving + e.fee)
            in UPKEEP -> f.copy(upkeep = f.upkeep + e.fee)
            Purpose.PROTECTION -> f.copy(protection = f.protection + e.fee)
            else -> f
        }
    }
    return f
}

/** What `events`, all done, cost: none if there are none. */
internal fun costs(events: List<StatEvent>): Costs? {
    if (events.isEmpty()) return null
    val fees = events.map { it.fee }
    val ratios = events.filter { it.amount > 0uL }.map { it.fee.toDouble() / it.amount.toDouble() * 1_000_000 }
    val times = events.mapNotNull { it.took?.toLong() }
    return Costs(
        count = events.size,
        usualFee = median(fees.map { it.toLong() }).toULong(),
        meanFee = (fees.sumOf { it.toDouble() } / fees.size).toULong(),
        usualPpm = ratios.takeIf { it.isNotEmpty() }?.let { medianOf(it).toLong() },
        meanPpm = ratios.takeIf { it.isNotEmpty() }?.let { (it.sum() / it.size).toLong() },
        usualSecs = times.takeIf { it.isNotEmpty() }?.let { median(it) },
        meanSecs = times.takeIf { it.isNotEmpty() }?.let { it.sum() / it.size },
    )
}

/** The middle of `values`, the mean of the two if there are an even number. */
internal fun median(values: List<Long>): Long {
    val sorted = values.sorted()
    val mid = sorted.size / 2
    return if (sorted.size % 2 == 1) sorted[mid] else (sorted[mid - 1] + sorted[mid]) / 2
}

private fun medianOf(values: List<Double>): Double {
    val sorted = values.sorted()
    val mid = sorted.size / 2
    return if (sorted.size % 2 == 1) sorted[mid] else (sorted[mid - 1] + sorted[mid]) / 2
}

/** A fee's share as a ratio in parts per million: `0.24%`, or null for none. */
internal fun ppmShare(ppm: Long?): String? =
    ppm?.takeIf { it > 0 }?.let { org.licash.wallet.share(it.toULong(), 1_000_000uL) }

/** A fee and its share: `12 sat · 0.24%`, `None` for nothing. */
internal fun cost(fee: ULong, ppm: Long?): String =
    if (fee == 0uL) "None" else sat(fee) + (ppmShare(ppm)?.let { " · $it" } ?: "")

/** A span of seconds as short as it can be said: `4 s`, `2 min`, `3 h`, `2 days`. */
internal fun took(seconds: Long): String = when {
    seconds < 1 -> "under a second"
    seconds < 90 -> "$seconds s"
    seconds < 90 * 60 -> "${(seconds + 30) / 60} min"
    seconds < 48 * 3600 -> "${(seconds + 1800) / 3600} h"
    else -> "${(seconds + 43_200) / 86_400} days"
}

/** `3 · 12,345 sat`: how many, and what they came to. */
internal fun tallied(t: Tally): String = "${digits(t.count.toULong())} · ${sat(t.amount)}"

/**
 * The instants to read the balance at across `span`, up to `now`: the
 * start, then `steps` more spread evenly to the end. All starts at the
 * first thing that happened, `first`, if any.
 */
internal fun instants(span: Span, now: Long, first: Long?, steps: Int = 24): List<Long> {
    val end = minOf(span.end, now)
    val start = if (span == EVER) (first ?: end) else span.start
    if (end <= start) return listOf(end)
    return (0..steps).map { start + (end - start) * it / steps }.distinct()
}

/** What the wallet held at the start and the end of a span, and how it changed, in sat. */
internal data class Change(val start: ULong, val end: ULong) {
    val up get() = end >= start
    val by get() = if (end >= start) end - start else start - end
}

internal val BalancePoint.total: ULong get() = licash + bitcoin

/** The change over `points`, none if there is no point. */
internal fun change(points: List<BalancePoint>): Change? =
    if (points.isEmpty()) null else Change(points.first().total, points.last().total)
