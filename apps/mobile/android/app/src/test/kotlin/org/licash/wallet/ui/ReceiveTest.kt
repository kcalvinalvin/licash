package org.licash.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class ReceiveTest {
    @Test
    fun the_ways_to_be_paid_are_those_the_hubs_make_possible() {
        assertEquals(listOf(Way.CODE, Way.BITCOIN), ways(lightning = false, silent = false))
        assertEquals(listOf(Way.LIGHTNING, Way.CODE, Way.BITCOIN), ways(lightning = true, silent = false))
        assertEquals(listOf(Way.CODE, Way.BITCOIN, Way.SILENT), ways(lightning = false, silent = true))
        assertEquals(
            listOf("Lightning invoice", "Licash code", "Bitcoin address", "Silent payment address"),
            ways(lightning = true, silent = true).map { it.label },
        )
    }

    @Test
    fun an_amount_is_asked_for_with_an_invoice_any_wallet_pays_where_a_hub_takes_lightning() {
        assertEquals(Way.LIGHTNING, picked(amount = 500, lightning = true))
        // Any amount, or no hub taking Lightning: the wallet's code.
        assertEquals(Way.CODE, picked(amount = null, lightning = true))
        assertEquals(Way.CODE, picked(amount = 500, lightning = false))
    }

    @Test
    fun an_invoice_asks_the_amount_entered_and_says_what_the_wallet_gets() {
        // No fee: nothing to say.
        assertNull(invoiceWords(4_000u, 4_000u, 4_000u))
        // The hub's fee in it.
        assertEquals("You get 3,997 sat, the hub's fee taken", invoiceWords(4_000u, 4_000u, 3_997u))
        // A sat short, where no amount and its fee come to it exactly.
        assertEquals(
            "The payer pays 100,001 sat\nYou get 100,000 sat, the hub's fee taken",
            invoiceWords(100_002u, 100_001u, 100_000u),
        )
    }
}
