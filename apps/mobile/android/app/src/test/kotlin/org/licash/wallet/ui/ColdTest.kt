package org.licash.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ColdTest {
    @Test
    fun an_xpub_needs_its_kind_of_address_chosen_and_a_zpub_does_not() {
        assertTrue(needsScript(" xpub6BgBgses"))
        assertTrue(needsScript("tpubD6NzVbkr"))
        assertFalse(needsScript("zpub6rFR7y4Q"))
        assertFalse(needsScript("vpub5Y6cjg78"))
        assertFalse(needsScript(""))
    }

    @Test
    fun rules_are_taken_only_when_each_makes_sense() {
        assertEquals(ColdForm(1_000_000uL, 0uL, 1.0), coldForm("1,000,000", "", "1"))
        assertEquals(ColdForm(500uL, 200uL, 0.5), coldForm("500", "200", "0.5"))
        // Keeping more than the threshold, or a fee limit of nothing or more than all.
        assertNull(coldForm("500", "600", "1"))
        assertNull(coldForm("500", "0", "0"))
        assertNull(coldForm("500", "0", "101"))
        assertNull(coldForm("", "0", "1"))
        assertNull(coldForm("five", "0", "1"))
    }
}
