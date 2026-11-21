package org.licash.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class WaitedTest {
    @Test
    fun saysNothingBeforeAnyWait() {
        val wait = Wait()
        wait.turn(false, 1_000)
        assertNull(wait.says(5_000))
    }

    @Test
    fun ticksWholeSecondsWhileWaiting() {
        val wait = Wait()
        wait.turn(true, 1_000)
        assertEquals("0 s", wait.says(1_999))
        assertEquals("12 s", wait.says(13_500))
        assertEquals("1 min 05 s", wait.says(66_000))
    }

    @Test
    fun saysHowLongItTookOnceOver() {
        val wait = Wait()
        wait.turn(true, 1_000)
        // Told again while still waiting: the start stays.
        wait.turn(true, 2_000)
        wait.turn(false, 4_250)
        assertEquals("Took 3.2 s", wait.says(9_000))
        wait.turn(false, 9_000)
        assertEquals("Took 3.2 s", wait.says(10_000))
    }

    @Test
    fun aNewWaitForgetsTheLast() {
        val wait = Wait()
        wait.turn(true, 0)
        wait.turn(false, 2_000)
        wait.turn(true, 5_000)
        assertEquals("1 s", wait.says(6_000))
    }

    @Test
    fun anUnnoticeableWaitGoesUnsaid() {
        val wait = Wait()
        wait.turn(true, 0)
        wait.turn(false, 99)
        assertNull(wait.says(200))
    }

    @Test
    fun lastedIsPreciseUnderTenSecondsThenClockLike() {
        assertEquals("0.4 s", lasted(450))
        assertEquals("9.9 s", lasted(9_999))
        assertEquals("12 s", lasted(12_300))
        assertEquals("2 min 05 s", lasted(125_000))
    }
}
