package org.licash.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.licash.core.Contract
import org.licash.core.ContractState
import org.licash.core.Rate
import org.licash.core.Notes
import org.licash.core.Recovery
import org.licash.core.RecoveryStage

class CoverTest {
    @Test
    fun a_corrected_recovery_shows_the_actual_sweep_not_the_original_claim() {
        val done = recovery(RecoveryStage.DONE, true, null).copy(corrected = 0uL, received = 2_500uL)
        assertEquals("2,500 sat back on Bitcoin", forcing(done, 1_000).says)
    }

    @Test
    fun note_count_counts_notes_not_their_denominations_across_hubs() {
        assertEquals(0uL, noteCount(emptyList()))
        assertEquals(7uL, noteCount(listOf(
            Notes("https://one.example", 64uL, 2uL),
            Notes("https://one.example", 4uL, 3uL),
            Notes("https://two.example", 32uL, 2uL),
        )))
    }

    private val created = 1_800_000_000L

    private fun contract(state: ContractState, mock: Boolean = true) = Contract(
        id = "c0", hub = "https://hub.example", state = state, mock = mock, cap = 131_071uL, price = 23_000uL, funded = 0uL, fundedReceived = false,
        paidFrom = 33_000uL, protectsUntil = 1_861u, closesAt = 2_005u, funding = null, createdAt = created,
        covers = 0uL, step = null, transfer = null,
    )

    @Test
    fun a_protection_says_where_it_stands_and_how_long_it_has_in_time() {
        val on = cover(contract(ContractState.Protecting), height = 1_000)
        assertEquals("Protecting", on.stands)
        assertTrue(on.on && on.runs && on.held && !on.failed)
        assertEquals("6 days left", on.left)
        assertEquals("7 days", on.ends)

        val opening = cover(contract(ContractState.Opening), height = 1_000)
        assertEquals("Setting up", opening.stands)
        assertTrue(opening.runs && !opening.on && !opening.held)

        val confirming = cover(contract(ContractState.Confirming), height = 1_000)
        assertEquals("Confirming", confirming.stands)
        assertTrue(confirming.runs && confirming.held && !confirming.on)
    }

    @Test
    fun a_genuine_one_is_live_never_protecting() {
        val live = cover(contract(ContractState.Protecting, mock = false), height = 1_000)
        assertEquals("Live", live.stands)
        assertTrue(live.on)
    }

    @Test
    fun one_past_protecting_has_nothing_left_but_still_ends() {
        val ending = cover(contract(ContractState.Ending), height = 1_900)
        assertEquals("Ending", ending.stands)
        assertTrue(ending.held && !ending.runs && !ending.on)
        assertNull(ending.left)
        assertEquals("18 hours", ending.ends)

        val ended = cover(contract(ContractState.Closed), height = 2_100)
        assertEquals("Ended", ended.stands)
        assertFalse(ended.held)
        assertNull(ended.left)
        assertNull(ended.ends)

        val failed = cover(contract(ContractState.Failed("the hub refused")), height = 1_000)
        assertEquals("Failed", failed.stands)
        assertTrue(failed.failed && !failed.held && !failed.runs)
        assertNull(failed.ends)
    }

    @Test
    fun no_time_is_said_while_the_height_is_unknown() {
        val on = cover(contract(ContractState.Protecting), height = 0)
        assertNull(on.left)
        assertNull(on.ends)
    }

    @Test
    fun ended_and_long_failed_ones_are_left_out_of_sight() {
        assertTrue(shown(contract(ContractState.Protecting), created + 1_000_000))
        assertTrue(shown(contract(ContractState.Ending), created + 1_000_000))
        assertFalse(shown(contract(ContractState.Closed), created))
        assertTrue(shown(contract(ContractState.Failed("no")), created + 86_399))
        assertFalse(shown(contract(ContractState.Failed("no")), created + 86_400))
    }

    @Test
    fun a_hub_is_named_by_its_host_an_onion_one_cut_short() {
        assertEquals("hub.example", name("https://hub.example"))
        assertEquals("127.0.0.1:38338", name("http://127.0.0.1:38338"))
        val onion = "abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx"
        assertEquals("abcdefgh….onion", name("http://$onion.onion"))
        assertEquals("abcdefgh….onion:8080", name("http://$onion.onion:8080"))
        assertEquals("short.onion", name("http://short.onion"))
    }

    @Test
    fun a_fee_is_said_as_short_as_it_can_be() {
        assertEquals("None", charge(Rate(0uL, 0uL)))
        assertEquals("10 sat", charge(Rate(10uL, 0uL)))
        assertEquals("0.5%", charge(Rate(0uL, 5_000uL)))
        assertEquals("10 sat + 0.5%", charge(Rate(10uL, 5_000uL)))
    }

    private fun recovery(stage: RecoveryStage, irrevocable: Boolean = false, nextAt: UInt? = 1_144u) = Recovery(
        id = "c0", stage = stage, irrevocable = irrevocable, claimed = 3_000uL, notes = 7u, request = null,
        publication = null, timeout = null, sweep = null, nextAt = nextAt, error = null,
    )

    @Test
    fun a_forced_close_says_where_it_stands_and_may_be_given_up_only_while_claimed() {
        val claimed = forcing(recovery(RecoveryStage.CLAIMED), height = 1_000)
        assertEquals("Forcing closed", claimed.stands)
        assertEquals("Final in 24 hours: it can be given up until then", claimed.says)
        assertTrue(claimed.revocable && !claimed.failed)
        // Final already: claimed, not to be given up.
        assertFalse(forcing(recovery(RecoveryStage.CLAIMED, irrevocable = true), 1_000).revocable)
        val requested = forcing(recovery(RecoveryStage.REQUESTED, irrevocable = true), 1_000)
        assertEquals("The hub has 24 hours to answer", requested.says)
        assertFalse(requested.revocable)
        // The chain's height unknown: no wait said.
        assertNull(forcing(recovery(RecoveryStage.CLAIMED), height = 0).says)
        assertEquals("3,000 sat back on Bitcoin", forcing(recovery(RecoveryStage.DONE, true, null), 1_000).says)
        assertTrue(forcing(recovery(RecoveryStage.LOST, true), 1_000).failed)
        // Closed with the hub, its fold awaited: signed, so not to be given up.
        val closing = forcing(recovery(RecoveryStage.FOLDING, irrevocable = true), 1_000)
        assertEquals("Closing", closing.stands)
        assertFalse(closing.revocable || closing.failed)
    }

    @Test
    fun canceling_requires_an_unsigned_claim_and_never_a_later_stage() {
        for (stage in RecoveryStage.entries) {
            assertEquals(stage == RecoveryStage.CLAIMED, forcing(recovery(stage), 0).revocable)
            assertFalse(forcing(recovery(stage, irrevocable = true), 0).revocable)
        }
    }

    @Test
    fun a_forced_close_the_hub_answered_awaits_the_hubs_proof_by_its_deadline() {
        val answered = recovery(RecoveryStage.ANSWERED, irrevocable = true, nextAt = null)
        assertEquals("The hub answered", forcing(answered, 1_000).says)
        assertFalse(forcing(answered, 1_000).failed)
        val owed = answered.copy(deadline = 1_144u, corrected = 1_000uL)
        assertEquals("Answered", forcing(owed, 1_000).stands)
        assertEquals(
            "The hub says 1,000 sat is owed: it must prove so within 24 hours, or you are paid your claim",
            forcing(owed, 1_000).says,
        )
        val asserted = forcing(recovery(RecoveryStage.ASSERTED, true).copy(corrected = 1_000uL), 1_000)
        assertEquals("The hub says 1,000 sat is owed: paid in 24 hours", asserted.says)
        assertEquals("Challenging", forcing(recovery(RecoveryStage.CHALLENGING, true), 1_000).stands)
        assertEquals("Back on Bitcoin in 24 hours", forcing(recovery(RecoveryStage.PAID, true), 1_000).says)
    }

    @Test
    fun a_payment_link_says_its_amount_in_bitcoin() {
        assertEquals("0.00033000", btc(33_000uL))
        assertEquals("1.00000001", btc(100_000_001uL))
    }
}
