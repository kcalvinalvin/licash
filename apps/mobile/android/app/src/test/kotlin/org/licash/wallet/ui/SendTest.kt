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

    @Test
    fun the_same_blocks_take_a_twentieth_of_the_time_on_mutinynet() {
        assertEquals("about 1 min", arrives(0u, 30u))
        assertEquals("about 1 min", arrives(1u, 30u))
        assertEquals("about 4 min", arrives(6u, 30u))
    }
}
