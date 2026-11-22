package org.licash.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.licash.core.ContractFit
import org.licash.core.DepositCoin

class ChooseContractTest {
    @Test
    fun durations_are_whole_days_within_the_range() {
        assertEquals(432u, blocksAt(400f, 432u, 52_560u))
        assertEquals(26_208u, blocksAt(26_250f, 432u, 52_560u))
        assertEquals(26_352u, blocksAt(26_300f, 432u, 52_560u))
        assertEquals(52_560u, blocksAt(52_560f, 432u, 52_560u))
        assertEquals(1_000u, blocksAt(1_000f, 200u, 1_000u))
        assertEquals(200u, blocksAt(-5f, 200u, 1_000u))
    }

    // Caps of 2^n - 1 sat a coin of 1,000,000 sat may take whole, as a hub
    // allowing 4 times the coin fits them: the smallest at `least` and above.
    private val caps = listOf(1_048_575uL, 2_097_151uL)

    private fun fake(least: ULong): ContractFit? =
        caps.firstOrNull { it >= least }?.let { ContractFit(cap = it, funded = 970_000uL) }

    @Test
    fun every_cap_a_coin_takes_whole_is_offered_smallest_first() {
        assertEquals(caps, fitsFrom(131_071uL, ::fake).map { it.cap })
        assertEquals(listOf<ULong>(), fitsFrom(131_071uL) { null }.map { it.cap })
        // A fit below what was asked is not taken for another.
        assertEquals(listOf<ULong>(), fitsFrom(5uL) { ContractFit(cap = 4uL, funded = 1uL) }.map { it.cap })
    }

    @Test
    fun the_cap_offered_is_half_again_the_coin_else_the_largest_allowed() {
        val fits = fitsFrom(131_071uL, ::fake)
        // 1.5 times 1,000,000 is past 1,048,575: the next.
        assertEquals(2_097_151uL, suggestedFit(fits, 1_000_000uL)?.cap)
        // 600,000 half again is 900,000: the smallest covers it.
        assertEquals(1_048_575uL, suggestedFit(fits, 600_000uL)?.cap)
        // None as large allowed: the largest.
        assertEquals(2_097_151uL, suggestedFit(fits, 1_500_000uL)?.cap)
        assertNull(suggestedFit(emptyList(), 1_000_000uL))
    }

    @Test
    fun the_cover_is_chosen_first_and_decides_the_coins_paying_it() {
        val small = DepositCoin("a:0", 600_000uL)
        val large = DepositCoin("b:0", 1_000_000uL)
        val tiny = DepositCoin("c:0", 1_000uL)
        fun fit(cap: ULong) = ContractFit(cap = cap, funded = 1uL)
        val fits = listOf(
            large to listOf(fit(1_048_575uL), fit(2_097_151uL)),
            small to listOf(fit(1_048_575uL)),
            tiny to emptyList(),
        )
        // Every cover some coin opens, each with the coins that pay it, smallest first.
        assertEquals(
            listOf(1_048_575uL to listOf(small, large), 2_097_151uL to listOf(large)),
            coversOf(fits),
        )
        // At first the one suggested for the smallest coin a contract takes whole.
        assertEquals(1_048_575uL, suggestedCover(fits))
        assertEquals(2_097_151uL, suggestedCover(listOf(large to fits[0].second)))
        assertNull(suggestedCover(listOf(tiny to emptyList())))
        assertEquals(emptyList<Pair<ULong, List<DepositCoin>>>(), coversOf(emptyList()))
    }

    @Test
    fun what_protection_asks_says_the_day_it_ends() {
        assertEquals("Open Licash now and then, and before it ends on May 21.", duty("May 21"))
    }
}
