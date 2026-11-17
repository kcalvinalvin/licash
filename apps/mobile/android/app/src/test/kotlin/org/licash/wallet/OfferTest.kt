package org.licash.wallet

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.licash.core.ContractQuote
import org.licash.core.DepositCoin

class OfferTest {
    private fun offer(vararg coins: String) = ContractQuote(
        hub = "https://hub.example",
        price = 23_000uL,
        cap = 4_096uL,
        blocks = 1_000u,
        protectsFor = 820u,
        mock = false,
        coin = 10_000_000uL,
        download = 100_100_000uL,
        range = null,
        afterAssert = 180u,
        coins = coins.mapIndexed { i, c -> DepositCoin(c, 10_000_000uL - i.toULong()) },
        suggestedCap = 15_000_000uL,
        suggestedBlocks = 26_280u,
        dust = 330uL,
        change = null,
    )

    @Test
    fun an_offer_is_for_the_largest_coin() {
        assertEquals("a:0", offeredCoin(offer("a:0", "b:1")))
        assertNull(offeredCoin(offer()))
    }

    @Test
    fun an_offer_declined_is_not_made_unasked_again_until_another_coin_comes() {
        assertTrue(offerDeclined(offer("a:0", "b:1"), "a:0"))
        // A larger coin paid since: offered again.
        assertFalse(offerDeclined(offer("c:0", "a:0"), "a:0"))
        // None declined, or none offered.
        assertFalse(offerDeclined(offer("a:0"), null))
        assertFalse(offerDeclined(null, "a:0"))
        assertFalse(offerDeclined(offer(), "a:0"))
    }
}
