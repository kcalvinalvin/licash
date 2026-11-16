package org.licash.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.licash.core.Rate

class FeesTest {
    @Test
    fun relative_fees_round_up_and_add_the_base_like_the_sdk() {
        assertEquals(0uL, relativeFee(0uL, 5_000uL))
        assertEquals(1uL, relativeFee(1uL, 1uL))
        assertEquals(50uL, relativeFee(10_000uL, 5_000uL))
        assertEquals(60uL, feeDue(10_000uL, Rate(10uL, 5_000uL)))
        assertEquals(11uL, feeDue(1uL, Rate(10uL, 1uL)))
        assertEquals(ULong.MAX_VALUE, feeDue(ULong.MAX_VALUE, Rate(ULong.MAX_VALUE, ULong.MAX_VALUE)))
    }

    @Test
    fun historical_fees_never_invent_a_rate_or_a_recipient_split() {
        val fee = recordedFee(1_000uL, 12uL)
        assertNull(fee.rate)
        assertTrue(fee.components.isEmpty())
        assertTrue(fee.explanation.contains("not saved"))
        assertEquals(0uL, recordedFee(1_000uL, 0uL).fee)
        assertTrue(recordedFee(1_000uL, 0uL).explanation.contains("may not be final"))
    }

    @Test
    fun a_payout_splits_its_fee_by_who_gets_it_leaving_out_parts_of_zero() {
        val quoted = payoutFee(10_000uL, 160uL, hub = 10uL, provider = 100uL, network = 50uL, quoted = true)
        assertEquals(listOf("Hub" to 10uL, "Payout provider" to 100uL, "Bitcoin network" to 50uL), quoted.components)
        assertTrue(quoted.explanation.contains("its own coin"))
        // Before its lock is made, the hub has charged nothing yet.
        val recorded = payoutFee(10_000uL, 150uL, hub = 0uL, provider = 100uL, network = 50uL, quoted = false)
        assertEquals(listOf("Payout provider" to 100uL, "Bitcoin network" to 50uL), recorded.components)
        assertTrue(recorded.explanation.contains("not saved"))
        assertNull(recorded.rate)
    }

    @Test
    fun parts_that_do_not_add_up_to_the_fee_are_not_shown() {
        assertTrue(payoutFee(10_000uL, 99uL, 10uL, 100uL, 50uL, quoted = false).components.isEmpty())
        assertTrue(payoutFee(10_000uL, 0uL, 0uL, 0uL, 0uL, quoted = true).components.isEmpty())
        assertTrue(feeParts(ULong.MAX_VALUE - 1uL, listOf("a" to ULong.MAX_VALUE, "b" to 1uL)).isEmpty())
        assertEquals(listOf("Hub" to 1uL, "Swap provider" to 4uL), swapFee(300uL, 5uL, hub = 1uL, provider = 4uL).components)
        assertEquals(recordedFee(300uL, 5uL), swapFee(300uL, 5uL, hub = 1uL, provider = 3uL))
    }

    @Test
    fun setup_cost_is_not_labeled_a_transaction_fee() {
        val cost = setupCost(4_096uL, 23_000uL)
        assertEquals("Total cost", cost.totalLabel)
        assertEquals("Setup cost", cost.title)
        assertNull(cost.rate)
    }
}
