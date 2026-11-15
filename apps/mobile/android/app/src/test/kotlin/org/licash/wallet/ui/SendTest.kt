package org.licash.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Test

class SendTest {
    @Test
    fun a_payment_on_bitcoin_arrives_a_block_after_those_awaited() {
        assertEquals("about 10 min", arrives(0u))
        assertEquals("about 20 min", arrives(1u))
        assertEquals("about 70 min", arrives(6u))
    }
}
