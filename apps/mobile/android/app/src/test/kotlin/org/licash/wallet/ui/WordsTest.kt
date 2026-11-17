package org.licash.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.licash.core.Balance

class WordsTest {
    private fun balance(notes: ULong = 0uL, onchain: ULong = 0uL, covered: ULong = 0uL, pending: ULong = 0uL) =
        Balance(
            notes = notes, notesPending = pending, onchain = onchain, moving = 0uL, kept = onchain, opening = 0uL,
            covered = covered,
        )

    @Test
    fun nothing_held_says_nothing_of_protection() {
        val none = protection(balance())
        assertNull(none.shield)
        assertNull(none.label)
        assertEquals(0, none.percent)
    }

    @Test
    fun what_is_on_bitcoin_or_covered_is_protected_and_the_rest_is_not() {
        val p = protection(balance(notes = 100_000uL, onchain = 10_000uL, covered = 4_096uL, pending = 9_294uL))
        assertEquals(14_096uL, p.protected)
        assertEquals(105_198uL, p.open)
        assertEquals(119_294uL, p.total)
        assertEquals(Shield.SOME, p.shield)
        assertEquals("12% protected", p.label)
    }

    @Test
    fun all_or_none_is_said_in_a_word_not_a_number() {
        val all = protection(balance(notes = 4_000uL, onchain = 1_000uL, covered = 4_000uL))
        assertEquals(Shield.ALL, all.shield)
        assertEquals("Protected", all.label)
        val none = protection(balance(notes = 4_000uL))
        assertEquals(Shield.NONE, none.shield)
        assertEquals("Not protected", none.label)
    }

    @Test
    fun a_share_is_never_rounded_to_all_or_none() {
        assertEquals(1, protection(balance(notes = 1_000_000uL, onchain = 1uL)).percent)
        assertEquals(99, protection(balance(notes = 1uL, onchain = 1_000_000uL)).percent)
        assertEquals(50, protection(balance(notes = 500uL, onchain = 500uL)).percent)
    }

    @Test
    fun covering_more_than_is_held_protects_no_more_than_is_held() {
        val p = protection(balance(notes = 1_000uL, covered = 4_096uL))
        assertEquals(1_000uL, p.protected)
        assertEquals(0uL, p.open)
        assertEquals(Shield.ALL, p.shield)
    }

    @Test
    fun blocks_are_told_as_time_never_as_heights() {
        assertEquals("now", span(0))
        assertEquals("now", span(-3))
        assertEquals("10 min", span(1))
        assertEquals("50 min", span(5))
        assertEquals("1 hour", span(6))
        assertEquals("2 hours", span(12))
        assertEquals("24 hours", span(144))
        assertEquals("6 days", span(861))
        assertEquals("7 days", span(1_001))
    }

    @Test
    fun a_day_is_said_as_short_as_today_lets_it() {
        val zone = java.time.ZoneId.of("UTC")
        val us = java.util.Locale.US
        // 2026-11-15 12:00 UTC.
        val now = 1_794_744_000L
        // Narrow or plain, the space before PM is the JDK's to choose.
        assertEquals("11:00 AM", day(now - 3_600, now, zone, us).replace('\u202f', ' '))
        assertEquals("Yesterday", day(now - 86_400, now, zone, us))
        assertEquals("Nov 13", day(now - 2 * 86_400, now, zone, us))
        assertEquals("Dec 31, 2025", day(1_767_182_400L, now, zone, us))
    }

    @Test
    fun paying_through_a_provider_says_how_long_its_sats_may_wait() {
        assertEquals(
            "If the provider declines or fails the payment, your sats come back by themselves, in about 33 hours.",
            refundWait(200u),
        )
        assertEquals("Not paid. Your money comes back by itself in about 2 hours.", notPaid(12u))
    }
}
