package org.licash.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.licash.core.ExtraCode

class ExtraCodeTest {
    private fun extra(index: UInt, label: String, payments: UInt = 0u) =
        ExtraCode(index, label, 0L, null, 0uL, payments, 0uL)

    @Test
    fun a_label_is_its_words_one_space_apart_within_its_length_and_none_if_blank() {
        assertNull(labelOf(" \t "))
        assertEquals("Bob online", labelOf("  Bob \n online "))
        assertEquals(MAX_CODE_LABEL, labelOf("x".repeat(MAX_CODE_LABEL + 5))!!.length)
    }

    @Test
    fun a_code_is_named_by_its_label_the_wallets_own_as_yours() {
        val codes = listOf(extra(1u, "Forum"), extra(2u, "Shop"))
        assertEquals("Main identity", codeName(0u, codes))
        assertEquals("Shop", codeName(2u, codes))
        // Not listed yet: by its number.
        assertEquals("Identity 3", codeName(3u, codes))
    }

    @Test
    fun what_a_code_took_is_said_in_payments() {
        assertEquals("Anonymous, nothing received yet", codeWords(extra(1u, "a")))
        assertEquals("Anonymous, 1 payment received", codeWords(extra(1u, "a", 1u)))
        assertEquals("Anonymous, 3 payments received", codeWords(extra(1u, "a", 3u)))
    }
}
