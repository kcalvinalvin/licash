package org.licash.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class ReceiveTest {
    @Test
    fun the_ways_to_be_paid_are_those_the_hubs_make_possible() {
        assertEquals(listOf(Way.CODE), ways(lightning = false, silent = false))
        assertEquals(listOf(Way.CODE, Way.LIGHTNING), ways(lightning = true, silent = false))
        assertEquals(listOf(Way.CODE, Way.SILENT), ways(lightning = false, silent = true))
        assertEquals(listOf("Bitcoin", "Lightning", "Silent"), ways(lightning = true, silent = true).map { it.label })
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
