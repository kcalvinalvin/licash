package org.licash.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.licash.core.Kind
import org.licash.core.Linking
import org.licash.core.NoteLine
import org.licash.core.NotesOf
import org.licash.core.NotesPaid
import org.licash.core.PaidTo

class FlowTest {
    private fun note(amount: ULong) = NoteLine(
        id = "n$amount", hub = "https://hub.example", amount = amount, madeBy = "p", madeByItself = false,
        spentBy = null, linking = Linking.UNLINKED, gone = false, addedAt = 0L,
    )

    private fun part(to: PaidTo, amount: ULong, notes: List<ULong>) =
        NotesPaid(to = to, hub = "https://hub.example", amount = amount, notes = notes)

    @Test
    fun a_note_twice_as_large_is_a_bit_longer() {
        assertEquals(0, bits(0uL))
        assertEquals(1, bits(1uL))
        assertEquals(13, bits(4_096uL))
        assertEquals(13, bits(8_191uL))
        assertEquals(14, bits(8_192uL))
        assertEquals(64, bits(ULong.MAX_VALUE))
    }

    @Test
    fun a_payment_is_drawn_payee_first_its_empty_parts_left_out() {
        val flow = flowOf(
            NotesOf(
                spent = listOf(note(4_096uL), note(2_048uL)),
                made = listOf(note(1_024uL)),
                paid = listOf(
                    part(PaidTo.KEPT, 1_024uL, listOf(1_024uL)),
                    part(PaidTo.HUB, 0uL, emptyList()),
                    part(PaidTo.PAYEE, 5_000uL, listOf(4_096uL, 512uL, 256uL, 128uL, 8uL)),
                ),
            ),
        )!!
        assertEquals(listOf(4_096uL, 2_048uL), flow.spent)
        assertEquals(listOf(PaidTo.PAYEE, PaidTo.KEPT), flow.out.map { it.to })
        assertEquals(5_000uL, flow.out[0].amount)
    }

    @Test
    fun received_none_spent_the_notes_made_are_the_wallets() {
        val flow = flowOf(NotesOf(spent = emptyList(), made = listOf(note(512uL), note(8uL)), paid = emptyList()))!!
        assertEquals(emptyList<ULong>(), flow.spent)
        assertEquals(listOf(Bucket(null, 520uL, listOf(512uL, 8uL))), flow.out)
    }

    @Test
    fun nothing_spent_or_made_draws_nothing() {
        assertNull(flowOf(NotesOf(spent = emptyList(), made = emptyList(), paid = emptyList())))
    }

    @Test
    fun parts_are_headed_in_a_word() {
        assertEquals("Provider", shortTo(Kind.PAYOUT, PaidTo.PAYEE))
        assertEquals("Invoice", shortTo(Kind.LIGHTNING, PaidTo.PAYEE))
        assertEquals("Payee", shortTo(Kind.SENT, PaidTo.PAYEE))
        assertEquals("Hub fee", shortTo(Kind.SENT, PaidTo.HUB))
        assertEquals("Yours", shortTo(Kind.RECEIVED, null))
    }
}
