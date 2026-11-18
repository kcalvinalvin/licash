package org.licash.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.licash.core.ChangeProblem
import org.licash.core.ChangeRange
import org.licash.core.ContractRange

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
    fun the_cap_slider_a_hair_off_an_end_is_at_it() {
        // Its thumb drawn at the start, a touch past it: the least, not 4,100.
        assertEquals(least, capAt(0.0005f, least, most))
        assertEquals(most, capAt(0.9999f, least, most))
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
    fun what_is_left_of_the_coin_is_covered_at_least() {
        // The 10,000,000 sat coin: what is left of it once paid, at least.
        assertEquals(9_902_000uL, coverAtLeast(4_096uL, 10_000_000uL, 98_000uL))
        // Little or nothing left: the hub's least.
        assertEquals(4_096uL, coverAtLeast(4_096uL, 25_000uL, 23_000uL))
        assertEquals(4_096uL, coverAtLeast(4_096uL, 0uL, 23_000uL))
    }

    @Test
    fun a_coin_that_cannot_turn_whole_into_protection_says_why() {
        assertEquals("Costs more than this coin holds", whyNot(ChangeProblem.Short(23_000uL)))
        assertTrue(whyNot(ChangeProblem.Uncovered(77_000uL)).contains("uncovered: cover more"))
        assertTrue(whyNot(ChangeProblem.TooMuch(9_977_000uL, 5_001_000uL)).contains("smaller coin"))
    }

    @Test
    fun the_coin_cashed_out_first_pays_and_is_taken_for_the_cap() {
        // A cap at most 2 times its coin, as the signet hubs take.
        val range = ContractRange(least, 33_554_431uL, 432u, 52_560u, 2u, 1_000uL, 5_000u)
        val taken = ChangeRange(11_000uL, 5_001_000uL)
        // The price itself, if the hub takes it for the cap: no change.
        assertEquals(3_000uL, coinFor(range, 6_000uL, 3_000uL, 330uL, taken))
        // A large cap: half of it, rounded up, its change taken into notes.
        assertEquals(500_001uL, coinFor(range, 1_000_001uL, 3_500uL, 330uL, taken))
        // Change less than the hub takes: as much more as that, covered.
        assertEquals(14_100uL, coinFor(range, 20_000uL, 3_100uL, 330uL, taken))
        // Not covered so: as it is, to refuse.
        assertEquals(2_048uL, coinFor(range, least, 1_100uL, 330uL, taken))
        // Under dust, to the fee: as it is.
        assertEquals(5_001uL, coinFor(range, 10_001uL, 5_000uL, 330uL, taken))
        assertEquals(500_001uL, coinFor(range, 1_000_001uL, 3_500uL, 330uL, null))
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
