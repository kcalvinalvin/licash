package org.licash.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.licash.core.ContractTerms
import org.licash.core.Hub
import org.licash.core.HubsFound
import org.licash.core.LightningTerms
import org.licash.core.Range
import org.licash.core.Rate

class HubsTest {
    private fun found(silent: UInt, onion: UInt) = HubsFound(hubs = emptyList(), silent = silent, silentOnion = onion)

    private fun hub(
        base: ULong = 0uL,
        ppm: ULong = 0uL,
        lightning: Boolean = false,
        contracts: Boolean = false,
        note: ULong = 0uL,
    ) = Hub(
        url = "https://hub.example", feeBase = base, feePpm = ppm, feeOutputMsat = note, deposits = null, silent = false,
        mailbox = true,
        lightningPay = null, lockFee = Rate(0uL, 0uL), refundFee = Rate(0uL, 0uL), depositFee = null,
        lightning = if (lightning) LightningTerms(Rate(2uL, 5_000uL), Rate(0uL, 0uL), Range(1uL, 1_000_000uL)) else null,
        contracts = if (contracts) ContractTerms(4_096uL, 23_000uL, 1_005u, true, null) else null,
    )

    @Test
    fun a_hub_is_told_by_its_fee_and_what_else_it_does() {
        assertEquals("No fee", about(hub()))
        assertEquals("Fee 0.5%", about(hub(ppm = 5_000uL)))
        assertEquals("Fee 1 sat + 0.5% · Lightning", about(hub(base = 1uL, ppm = 5_000uL, lightning = true)))
        assertEquals("No fee · Lightning · Protection", about(hub(lightning = true, contracts = true)))
    }

    @Test
    fun a_fee_on_the_notes_a_payment_makes_is_said_beside_its_rate() {
        assertEquals("None", paying(hub()))
        assertEquals("1 sat + 0.01%", paying(hub(base = 1uL, ppm = 100uL)))
        assertEquals("1 sat + 0.01% + 0.1 sat a note", paying(hub(base = 1uL, ppm = 100uL, note = 100uL)))
        assertEquals("2 sat a note", paying(hub(note = 2_000uL)))
        assertEquals("1.25 sat a note", paying(hub(note = 1_250uL)))
        assertEquals("Fee 0.1 sat a note", about(hub(note = 100uL)))
    }

    @Test
    fun the_most_a_payment_costs_counts_the_notes_it_may_make() {
        // 1 sat and 0.01% of 10,000, and 28 notes at a tenth of a sat each.
        assertEquals(2uL, payingUpTo(hub(base = 1uL, ppm = 100uL), 10_000uL))
        assertEquals(5uL, payingUpTo(hub(base = 1uL, ppm = 100uL, note = 100uL), 10_000uL))
        assertEquals(0uL, payingUpTo(hub(note = 100uL), 0uL))
        assertEquals(0uL, payingUpTo(hub(), 10_000uL))
    }

    @Test
    fun hubs_that_all_answered_leave_nothing_to_say() {
        assertNull(unanswered(found(0u, 0u), tor = false))
    }

    @Test
    fun those_that_did_not_answer_are_counted_and_tor_offered_when_it_would_reach_them() {
        assertEquals("1 more did not answer", unanswered(found(1u, 0u), tor = false))
        assertEquals("3 more did not answer", unanswered(found(3u, 0u), tor = false))
        assertEquals("3 more did not answer: try with Tor on", unanswered(found(3u, 2u), tor = false))
        assertEquals("3 more did not answer", unanswered(found(3u, 2u), tor = true))
    }
}
