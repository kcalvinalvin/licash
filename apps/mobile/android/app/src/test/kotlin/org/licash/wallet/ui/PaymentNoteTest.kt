package org.licash.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class PaymentNoteTest {
    @Test
    fun a_note_is_its_words_one_space_apart_and_none_if_blank() {
        assertNull(noteOf(""))
        assertNull(noteOf(" \n\t "))
        assertEquals("for the pizza", noteOf("  for the \n pizza "))
    }

    @Test
    fun a_note_typed_is_cut_to_whole_characters_within_its_bytes() {
        assertEquals("x".repeat(MAX_NOTE_BYTES), fitNote("x".repeat(MAX_NOTE_BYTES + 3)))
        // Two bytes each: a whole number of them.
        assertEquals("é".repeat(MAX_NOTE_BYTES / 2), fitNote("é".repeat(MAX_NOTE_BYTES)))
        // Four bytes each, two chars in Kotlin: never split.
        val emoji = String(Character.toChars(0x1F355))
        val fitted = fitNote("a" + emoji.repeat(60))
        assertEquals(1 + 4 * 49, fitted.toByteArray(Charsets.UTF_8).size)
        assertEquals("short", fitNote("short"))
    }
}
