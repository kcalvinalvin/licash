package org.licash.wallet.ui

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Test
import org.licash.wallet.digits

class AmountTest {
    @Test
    fun only_digits_are_typed_no_leading_zeros_at_most_sixteen() {
        assertEquals("5000", amountDigits("5,000 sat"))
        assertEquals("12", amountDigits("0012"))
        assertEquals("", amountDigits("000"))
        assertEquals("", amountDigits("-.,"))
        assertEquals("1234567890123456", amountDigits("12345678901234567"))
    }

    @Test
    fun thousands_are_set_apart_as_amounts_show_elsewhere() {
        for (n in listOf(1L, 12L, 123L, 1_234L, 12_345L, 123_456L, 1_234_567L, 2_100_000_000_000_000L)) {
            assertEquals(digits(n.toULong()), grouped(n.toString()).text)
        }
        assertEquals("", grouped("").text)
    }

    @Test
    fun each_offset_lands_by_its_digit_past_the_commas() {
        // "1,234": before 1, before 2 (past the comma), before 3, before 4, past 4.
        assertArrayEquals(intArrayOf(0, 2, 3, 4, 5), grouped("1234").at)
        assertArrayEquals(intArrayOf(0, 1, 2, 3), grouped("123").at)
        assertArrayEquals(intArrayOf(0, 1, 3, 4, 5, 7, 8, 9, 10), grouped("12345678").at)
        assertArrayEquals(intArrayOf(0), grouped("").at)
    }
}
