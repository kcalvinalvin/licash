package org.licash.wallet.ui

import java.time.DayOfWeek
import java.time.ZoneId
import java.time.ZonedDateTime
import java.util.Locale
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.licash.core.BalancePoint
import org.licash.core.Flow
import org.licash.core.StatEvent
import org.licash.core.State
import org.licash.core.Unlink
import org.licash.core.Way

class StatsFiguresTest {
    private val utc = ZoneId.of("UTC")
    private val madrid = ZoneId.of("Europe/Madrid")

    private fun at(zone: ZoneId, y: Int, m: Int, d: Int, h: Int = 0): Long =
        ZonedDateTime.of(y, m, d, h, 0, 0, 0, zone).toEpochSecond()

    private fun event(
        flow: Flow,
        way: Way = Way.LICASH,
        amount: ULong = 1_000uL,
        fee: ULong = 0uL,
        state: State = State.DONE,
        time: Long? = 100L,
        took: ULong? = null,
        hub: ULong? = null,
        provider: ULong? = null,
        network: ULong? = null,
    ) = StatEvent(
        id = "e", flow = flow, way = way, state = state, at = time, took = took,
        amount = amount, fee = fee, hubFee = hub, providerFee = provider, networkFee = network,
    )

    @Test
    fun a_week_starts_on_the_first_day_and_a_month_on_the_first() {
        // Wednesday Nov 19, 2025.
        val now = at(utc, 2025, 11, 19, 15)
        assertEquals(Span(at(utc, 2025, 11, 17), at(utc, 2025, 11, 24)), span(Period.WEEK, now, 0, utc, DayOfWeek.MONDAY))
        assertEquals(Span(at(utc, 2025, 11, 16), at(utc, 2025, 11, 23)), span(Period.WEEK, now, 0, utc, DayOfWeek.SUNDAY))
        assertEquals(Span(at(utc, 2025, 11, 10), at(utc, 2025, 11, 17)), span(Period.WEEK, now, 1, utc, DayOfWeek.MONDAY))
        assertEquals(Span(at(utc, 2025, 11, 1), at(utc, 2025, 12, 1)), span(Period.MONTH, now, 0, utc))
        assertEquals(Span(at(utc, 2025, 1, 1), at(utc, 2026, 1, 1)), span(Period.YEAR, now, 0, utc))
        assertEquals(Span(at(utc, 2024, 1, 1), at(utc, 2025, 1, 1)), span(Period.YEAR, now, 1, utc))
        assertEquals(Span(at(utc, 2025, 11, 19), at(utc, 2025, 11, 20)), span(Period.DAY, now, 0, utc))
        assertEquals(EVER, span(Period.ALL, now, 0, utc))
    }

    @Test
    fun a_month_goes_back_over_a_year_and_a_day_over_a_month() {
        val now = at(utc, 2026, 1, 31, 12)
        assertEquals(Span(at(utc, 2025, 12, 1), at(utc, 2026, 1, 1)), span(Period.MONTH, now, 1, utc))
        assertEquals(Span(at(utc, 2025, 1, 1), at(utc, 2025, 2, 1)), span(Period.MONTH, now, 12, utc))
        assertEquals(Span(at(utc, 2025, 12, 31), at(utc, 2026, 1, 1)), span(Period.DAY, now, 31, utc))
    }

    @Test
    fun a_day_is_the_users_own_and_is_23_hours_long_when_the_clocks_go_forward() {
        // Madrid, Sunday March 29, 2026: 02:00 became 03:00.
        val now = at(madrid, 2026, 3, 29, 12)
        val day = span(Period.DAY, now, 0, madrid)
        assertEquals(at(madrid, 2026, 3, 29), day.start)
        assertEquals(23 * 3600L, day.end - day.start)
        assertTrue(now in day)
        assertTrue(day.end !in day)
        assertEquals(Span(at(madrid, 2026, 3, 1), at(madrid, 2026, 4, 1)), span(Period.MONTH, now, 0, madrid))
    }

    @Test
    fun a_period_is_called_what_it_is_near_to_now() {
        val now = at(utc, 2025, 11, 19, 15)
        val say = { p: Period, back: Int -> label(p, now, back, utc, Locale.US) }
        assertEquals("Today", say(Period.DAY, 0))
        assertEquals("Yesterday", say(Period.DAY, 1))
        assertEquals("Nov 12", say(Period.DAY, 7))
        assertEquals("Oct 20, 2024", label(Period.DAY, now, 395, utc, Locale.US))
        assertEquals("This week", say(Period.WEEK, 0))
        assertEquals("Last week", say(Period.WEEK, 1))
        assertEquals("Nov 3 – 9", say(Period.WEEK, 2))
        assertEquals("Oct 27 – Nov 2", say(Period.WEEK, 3))
        assertEquals("This month", say(Period.MONTH, 0))
        assertEquals("September", say(Period.MONTH, 2))
        assertEquals("December 2024", say(Period.MONTH, 11))
        assertEquals("This year", say(Period.YEAR, 0))
        assertEquals("2023", say(Period.YEAR, 2))
        assertEquals("All time", say(Period.ALL, 0))
    }

    @Test
    fun the_usual_is_the_middle_and_the_average_is_the_mean() {
        assertEquals(5L, median(listOf(9L, 1L, 5L)))
        assertEquals(5L, median(listOf(1L, 4L, 6L, 100L)))
        // Three sends of 1 sat on 1,000, and one stuck at 1,000.
        val sends = listOf(1uL, 1uL, 1uL, 1_000uL).map { event(Flow.SENT, fee = it, took = if (it == 1_000uL) 600uL else 2uL) }
        val c = costs(sends)!!
        assertEquals(4, c.count)
        assertEquals(1uL, c.usualFee)
        assertEquals(250uL, c.meanFee)
        assertEquals(1_000L, c.usualPpm)
        assertEquals(250_750L, c.meanPpm)
        assertEquals(2L, c.usualSecs)
        assertEquals(151L, c.meanSecs)
        assertNull(costs(emptyList()))
    }

    @Test
    fun what_has_no_amount_has_no_share_and_what_was_not_timed_has_no_time() {
        val c = costs(listOf(event(Flow.OPTIMIZED, amount = 0uL, fee = 1uL)))!!
        assertEquals(1uL, c.usualFee)
        assertNull(c.usualPpm)
        assertNull(c.usualSecs)
        assertEquals("1 sat", cost(c.usualFee, c.usualPpm))
        assertEquals("None", cost(0uL, null))
        assertEquals("12 sat · 0.24%", cost(12uL, 2_400L))
    }

    @Test
    fun a_period_counts_what_was_done_in_it_by_way_and_leaves_out_the_rest() {
        val events = listOf(
            event(Flow.RECEIVED, Way.LICASH, 500uL, time = 100),
            event(Flow.RECEIVED, Way.LICASH, 700uL, time = 200),
            event(Flow.RECEIVED, Way.BITCOIN, 3_000uL, time = 150),
            event(Flow.RECEIVED, Way.LIGHTNING, 90uL, time = 5_000), // another period
            event(Flow.SENT, Way.LIGHTNING, 200uL, fee = 2uL, hub = 2uL, time = 120),
            event(Flow.SENT, Way.LIGHTNING, 300uL, fee = 0uL, state = State.FAILED, time = 130),
            event(Flow.SENT, Way.LICASH, 400uL, fee = 0uL, state = State.PENDING, time = 140),
        )
        val s = summarize(events, emptyList(), Span(0, 1_000))
        assertEquals(Tally(2, 1_200uL), s.received[Way.LICASH])
        assertEquals(Tally(1, 3_000uL), s.received[Way.BITCOIN])
        assertNull(s.received[Way.LIGHTNING])
        assertEquals(Tally(1, 200uL), s.sent[Way.LIGHTNING])
        assertNull(s.sent[Way.LICASH])
        assertEquals(1, s.failed)
        assertEquals(6, s.events)
        assertEquals(2uL, s.fees.total)
        // All time has all of it.
        assertEquals(7, summarize(events, emptyList(), EVER).events)
    }

    @Test
    fun a_coin_whose_block_time_is_not_known_counts_in_all_time_only() {
        val events = listOf(event(Flow.RECEIVED, Way.BITCOIN, 3_000uL, time = null))
        assertEquals(0, summarize(events, emptyList(), Span(0, 1_000)).events)
        assertEquals(Tally(1, 3_000uL), summarize(events, emptyList(), EVER).received[Way.BITCOIN])
    }

    @Test
    fun fees_are_split_by_purpose_and_by_whom_and_what_is_not_itemized_is_said() {
        val events = listOf(
            event(Flow.SENT, Way.BITCOIN, fee = 100uL, hub = 1uL, provider = 4uL, network = 95uL),
            event(Flow.SENT, Way.LIGHTNING, fee = 2uL, hub = 2uL),
            event(Flow.MOVED, Way.LICASH, fee = 5uL, hub = 1uL, provider = 4uL),
            event(Flow.MOVED, Way.BITCOIN, fee = 100uL, hub = 100uL),
            event(Flow.OPTIMIZED, amount = 0uL, fee = 1uL, hub = 1uL),
            event(Flow.RENEWED, fee = 3uL, hub = 3uL),
            event(Flow.NORMALIZED, fee = 1uL, hub = 1uL),
            // A contract's price: no part of it is told.
            event(Flow.PROTECTION, Way.BITCOIN, amount = 10_000uL, fee = 50uL),
            // Nothing paid for what failed or came.
            event(Flow.SENT, fee = 0uL, state = State.FAILED),
            event(Flow.RECEIVED, fee = 0uL),
        )
        val f = fees(events)
        assertEquals(262uL, f.total)
        assertEquals(mapOf(Way.BITCOIN to 100uL, Way.LIGHTNING to 2uL), f.sending)
        assertEquals(105uL, f.moving)
        assertEquals(5uL, f.upkeep)
        assertEquals(50uL, f.protection)
        assertEquals(109uL, f.hub)
        assertEquals(8uL, f.provider)
        assertEquals(95uL, f.network)
        assertEquals(50uL, f.unsplit)
        assertEquals(f.total, f.hub + f.provider + f.network + f.unsplit)
    }

    @Test
    fun each_way_of_paying_has_its_costs_and_so_has_note_upkeep_and_protection() {
        val events = listOf(
            event(Flow.SENT, Way.LICASH, 1_000uL, fee = 1uL, took = 1uL),
            event(Flow.SENT, Way.LIGHTNING, 1_000uL, fee = 5uL, took = 4uL),
            event(Flow.SENT, Way.LIGHTNING, 1_000uL, fee = 7uL, took = 8uL),
            event(Flow.OPTIMIZED, amount = 0uL, fee = 1uL, took = 2uL),
            event(Flow.PROTECTION, Way.BITCOIN, amount = 100_000uL, fee = 500uL),
        )
        val s = summarize(events, emptyList(), EVER)
        assertEquals(1uL, s.sending[Way.LICASH]!!.usualFee)
        assertEquals(6uL, s.sending[Way.LIGHTNING]!!.usualFee)
        assertEquals(6L, s.sending[Way.LIGHTNING]!!.usualSecs)
        assertNull(s.sending[Way.BITCOIN])
        assertEquals(2L, s.optimizing!!.usualSecs)
        assertEquals(Tally(1, 100_000uL), s.covered)
        assertEquals(500uL, s.protecting!!.usualFee)
        assertEquals(5_000L, s.protecting.usualPpm)
        assertEquals("0.50%", ppmShare(s.protecting.usualPpm))
    }

    @Test
    fun notes_unlinked_in_a_period_are_timed_by_their_wait() {
        val unlinks = listOf(
            Unlink(at = 100L, waited = 60uL, amount = 1uL),
            Unlink(at = 200L, waited = 3_600uL, amount = 1uL),
            Unlink(at = 300L, waited = 120uL, amount = 1uL),
            Unlink(at = 9_000L, waited = 99uL, amount = 1uL),
        )
        val s = summarize(emptyList(), unlinks, Span(0, 1_000))
        assertEquals(3, s.unlinking!!.first)
        assertEquals(120L, s.unlinking.second.first)
        assertEquals(1_260L, s.unlinking.second.second)
        assertNull(summarize(emptyList(), emptyList(), EVER).unlinking)
    }

    @Test
    fun a_time_is_said_as_short_as_it_can_be() {
        assertEquals("under a second", took(0))
        assertEquals("4 s", took(4))
        assertEquals("89 s", took(89))
        assertEquals("2 min", took(100))
        assertEquals("45 min", took(2_700))
        assertEquals("3 h", took(3 * 3_600L))
        assertEquals("2 days", took(2 * 86_400L))
    }

    @Test
    fun the_balance_is_read_from_the_start_of_a_period_to_its_end_or_now() {
        val span = Span(1_000, 2_000)
        assertEquals((0..4).map { 1_000L + it * 250 }, instants(span, now = 5_000, first = null, steps = 4))
        // A period still going is read up to now.
        assertEquals(listOf(1_000L, 1_500L), instants(span, now = 1_500, first = null, steps = 1))
        // One not begun is read once.
        assertEquals(listOf(500L), instants(span, now = 500, first = null))
        // All starts at the first thing that happened.
        assertEquals(listOf(100L, 600L), instants(EVER, now = 600, first = 100, steps = 1))
        assertEquals(listOf(600L), instants(EVER, now = 600, first = null))
    }

    @Test
    fun the_balance_changes_from_its_first_point_to_its_last() {
        val point = { at: Long, licash: ULong, bitcoin: ULong -> BalancePoint(at, licash, bitcoin, emptyList()) }
        val c = change(listOf(point(0, 100uL, 50uL), point(1, 500uL, 0uL), point(2, 90uL, 10uL)))!!
        assertEquals(Change(150uL, 100uL), c)
        assertEquals(false, c.up)
        assertEquals(50uL, c.by)
        assertNull(change(emptyList()))
    }
}
