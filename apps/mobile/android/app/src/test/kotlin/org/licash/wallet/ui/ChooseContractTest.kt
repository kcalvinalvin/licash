package org.licash.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class ChooseContractTest {
    private val least = 4_096uL
    private val most = 20_000_000uL

    @Test
    fun the_cap_slider_reaches_both_ends_exactly() {
        assertEquals(least, capAt(0f, least, most))
        assertEquals(most, capAt(1f, least, most))
        assertEquals(least, capAt(0.5f, least, least))
        assertEquals(0f, capPosition(least, least, most))
        assertEquals(1f, capPosition(most, least, most))
    }

    @Test
    fun caps_between_are_round_and_where_the_slider_shows_them() {
        // Half again a 10,000,000 sat coin, as the app suggests at first.
        val suggested = 15_000_000uL
        assertEquals(suggested, capAt(capPosition(suggested, least, most), least, most))
        for (cap in listOf(10_000uL, 250_000uL, 1_000_000uL, 4_700_000uL)) {
            assertEquals(cap, capAt(capPosition(cap, least, most), least, most))
        }
    }

    @Test
    fun the_cap_grows_with_the_slider_within_the_range() {
        var last = 0uL
        for (step in 0..1000) {
            val cap = capAt(step / 1000f, least, most)
            assertTrue("$cap after $last at $step", cap >= last)
            assertTrue(cap in least..most)
            last = cap
        }
        // Evenly by ratio: halfway is about the geometric mean, not the middle.
        assertEquals(290_000uL, capAt(0.5f, least, most))
    }

    @Test
    fun durations_are_whole_days_within_the_range() {
        assertEquals(432u, blocksAt(400f, 432u, 52_560u))
        assertEquals(26_208u, blocksAt(26_250f, 432u, 52_560u))
        assertEquals(26_352u, blocksAt(26_300f, 432u, 52_560u))
        assertEquals(52_560u, blocksAt(52_560f, 432u, 52_560u))
        assertEquals(1_000u, blocksAt(1_000f, 200u, 1_000u))
        assertEquals(200u, blocksAt(-5f, 200u, 1_000u))
    }
}
