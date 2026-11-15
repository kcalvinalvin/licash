package org.licash.wallet

import org.junit.Assert.assertEquals
import org.junit.Test

class FormatTest {
    @Test
    fun a_reason_is_a_sentence_with_its_amounts_grouped() {
        assertEquals(
            "Hub takes at most 5,000,000 sat, not 12,000,000 sat",
            plain("hub takes at most 5000000 sat, not 12000000 sat"),
        )
        // Short amounts and other numbers stay as they are.
        assertEquals("Lock expires at block 1200, 500 sat", plain("lock expires at block 1200, 500 sat"))
    }

    @Test
    fun a_fee_says_what_it_is_of_the_amount() {
        assertEquals(null, share(0uL, 100uL))
        assertEquals(null, share(5uL, 0uL))
        assertEquals("<0.01%", share(1uL, 1_000_000uL))
        assertEquals("0.24%", share(24uL, 10_000uL))
        assertEquals("1%", share(1uL, 100uL))
        assertEquals("1.2%", share(12uL, 1_000uL))
        assertEquals("5%", share(1uL, 20uL))
        assertEquals("10%", share(999uL, 10_000uL))
        assertEquals("13%", share(1uL, 8uL))
        assertEquals("3,000%", share(30uL, 1uL))
        assertEquals("12 sat · 0.24%", feeOf(12uL, 5_000uL))
        assertEquals("0 sat", feeOf(0uL, 5_000uL))
    }

    @Test
    fun no_provider_is_said_without_naming_providers() {
        assertEquals(
            "No one can send 20,000 sat on Bitcoin right now. Try less, or later.",
            plain("no swap provider found pays 20000 sat out on Bitcoin now"),
        )
        assertEquals(
            "No one can move 1,500 sat between your hubs right now. Try less, or later.",
            plain("no swap provider found moves 1500 sat from http://a:1 to http://b:2 now"),
        )
    }
}
