package org.licash.wallet

import org.junit.Assert.assertEquals
import org.junit.Test
import org.licash.core.ContractSafety
import org.licash.core.Network

class SafetyTest {
    @Test fun aPhoneOwnsItsMarginAndDaysAreOnlyAnEstimate() {
        assertEquals(2_016u, exitMarginBlocks(14, Network.SIGNET))
        assertEquals(1_008u, exitMarginBlocks(7, Network.BITCOIN))
        assertEquals(40_320u, exitMarginBlocks(14, Network.MUTINYNET))
    }

    @Test fun theSignedChallengeWindowIsShownAsEstimatedHours() {
        assertEquals(24u, checkInHours(144u, Network.SIGNET))
        assertEquals(1u, checkInHours(6u, Network.SIGNET))
        assertEquals(1u, checkInHours(144u, Network.MUTINYNET))
    }

    @Test fun equalityStillFitsAndOneBlockLaterIsInsideTheMargin() {
        val s = ContractSafety(7_966u, 10_000u, 12u, 2_016u, 6u, 144u)
        assertEquals("Request should confirm by block 7966", safetyStatus(s, 7_966))
        assertEquals("Inside your recovery margin", safetyStatus(s, 7_967))
        assertEquals("Chain height unknown", safetyStatus(s, 0))
        assertEquals("Margin does not fit this contract", safetyStatus(s.copy(requestBy = null), 100))
    }
}
