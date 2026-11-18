package org.licash.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.licash.core.ContractRange

class ChooseContractTest {
    private val sizes = (17..25).map { (1uL shl it) - 1uL }

    @Test
    fun a_cover_is_one_of_the_hubs_sizes_the_largest_up_to_what_is_asked() {
        assertEquals(131_071uL, sizeAtMost(sizes, 131_071uL))
        // Half again a 10,000,000 sat coin, as the app suggests at first.
        assertEquals(8_388_607uL, sizeAtMost(sizes, 15_000_000uL))
        assertEquals(33_554_431uL, sizeAtMost(sizes, ULong.MAX_VALUE))
        // Below the least: the least.
        assertEquals(131_071uL, sizeAtMost(sizes, 4_096uL))
        assertEquals(5uL, sizeAtMost(emptyList(), 5uL))
    }

    @Test
    fun her_part_follows_its_slider_to_both_ends_exactly() {
        assertEquals(0uL, fundedAt(0f, 9_000_000uL))
        assertEquals(9_000_000uL, fundedAt(1f, 9_000_000uL))
        assertEquals(4_500_000uL, fundedAt(0.5f, 9_000_000uL))
        assertEquals(0uL, fundedAt(0.7f, 0uL))
        var last = 0uL
        for (step in 0..1000) {
            val part = fundedAt(step / 1000f, 9_000_000uL)
            assertTrue("$part after $last at $step", part >= last && part <= 9_000_000uL)
            last = part
        }
    }

    @Test
    fun a_coin_that_cannot_pay_says_why() {
        // A cover at most 2 times its coin, as the signet hubs take.
        val range = ContractRange(131_071uL, 33_554_431uL, 432u, 52_560u, 2u, 20_000uL, 3_000uL, 5_000u, 0u)
        assertEquals("Costs more than this coin holds", whyNot(range, 131_071uL, 20_000uL))
        assertTrue(whyNot(range, 1_048_575uL, 500_000uL).contains("too small for so large a cover"))
        assertEquals("Costs more than this coin holds", whyNot(range, 1_048_575uL, 600_000uL))
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
