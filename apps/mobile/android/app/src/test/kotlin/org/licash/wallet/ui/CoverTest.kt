package org.licash.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.licash.core.Contract
import org.licash.core.ContractState
import org.licash.core.Rate

class CoverTest {
    private val created = 1_800_000_000L

    private fun contract(state: ContractState, mock: Boolean = true) = Contract(
        id = "c0", hub = "https://hub.example", state = state, mock = mock, cap = 4_096uL, price = 23_000uL,
        paidFrom = 33_000uL, protectsUntil = 1_861u, closesAt = 2_005u, funding = null, createdAt = created,
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

    @Test
    fun the_coin_to_cash_out_first_pays_the_price_and_leaves_change() {
        assertEquals(33_000uL, firstCoin(23_000uL))
        assertEquals("0.00033000", btc(33_000uL))
        assertEquals("1.00000001", btc(100_000_001uL))
    }
}
