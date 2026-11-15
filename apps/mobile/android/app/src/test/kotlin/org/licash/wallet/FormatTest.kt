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
