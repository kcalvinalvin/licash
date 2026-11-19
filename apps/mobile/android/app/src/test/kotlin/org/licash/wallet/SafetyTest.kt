package org.licash.wallet

import org.junit.Assert.assertEquals
import org.junit.Test
import org.licash.core.ContractSafety

class SafetyTest {
    @Test fun aPhoneOwnsItsMarginAndDaysAreOnlyAnEstimate() {
        // Counted in blocks, 144 a day on every network: Mutinynet's too.
        assertEquals(2_016u, exitMarginBlocks(14))
        assertEquals(1_008u, exitMarginBlocks(7))
    }

    @Test fun theSignedChallengeWindowIsShownAsEstimatedHours() {
        assertEquals(24u, checkInHours(144u, 600u))
        assertEquals(1u, checkInHours(6u, 600u))
        // The same blocks on Mutinynet, 30 seconds each: 72 minutes.
        assertEquals(1u, checkInHours(144u, 30u))
        assertEquals(24u, checkInHours(2_880u, 30u))
    }

    @Test fun equalityStillFitsAndOneBlockLaterIsInsideTheMargin() {
        val s = ContractSafety(7_966u, 10_000u, 12u, 2_016u, 6u, 144u)
        assertEquals("Request should confirm by block 7966", safetyStatus(s, 7_966))
        assertEquals("Inside your recovery margin", safetyStatus(s, 7_967))
        assertEquals("Chain height unknown", safetyStatus(s, 0))
        assertEquals("Margin does not fit this contract", safetyStatus(s.copy(requestBy = null), 100))
    }
}
