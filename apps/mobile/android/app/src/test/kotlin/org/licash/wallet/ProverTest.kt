package org.licash.wallet

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ProverTest {
    private val first = listOf(
        "== run 1763260000 po2 18, 8 cpus",
        "native-claim: executed in 2.1 s, 89463558 user cycles, 4 segments; work in work",
        "native-claim: segment 1/4 proved and lifted in 100.0 s (rss 68472 kB, peak 2417940 kB)",
        "native-claim: segment 2/4 proved and lifted in 120.0 s (rss 70296 kB, peak 2430208 kB)",
        "== exit 137",
    )

    @Test
    fun a_proof_killed_part_way_says_how_far_it_got_and_why_it_stopped() {
        val p = progressOf(first)
        assertEquals(4, p.segments)
        assertEquals(2, p.lifted)
        assertEquals(0, p.joined)
        assertEquals("the prover quit (exit 137)", p.error)
        assertEquals(110.0, p.segmentSecs!!, 1e-9)
        // Two segments of four, none of three joins: 2 / (4 + 0.3 * 3).
        assertEquals(2 / 4.9f, p.share, 1e-6f)
        // Two segments at 110 s, three joins at a third of that.
        assertEquals(220L + 99L, p.secsLeft)
        assertEquals("Stopped: the prover quit (exit 137)", provingWords(p))
        assertEquals("Paused at 40%", provingWords(p.copy(paused = true)))
    }

    @Test
    fun a_proof_carried_on_counts_what_it_had_and_ends_with_its_receipt() {
        val again = first + listOf(
            "== run 1763270000 po2 18, 8 cpus",
            "native-claim: carried on, 2 of 4 segments lifted already; work in work",
            "native-claim: segment 3/4 proved and lifted in 90.0 s (rss 1 kB, peak 2 kB)",
        )
        val running = progressOf(again).copy(running = true)
        assertNull(running.error)
        assertEquals(3, running.lifted)
        assertEquals("Proving part 4 of 4, about 3 min left", provingWords(running))
        val joining = progressOf(
            again + listOf(
                "native-claim: segment 4/4 proved and lifted in 90.0 s (rss 1 kB, peak 2 kB)",
                "native-claim: joined 2/4 in 30.0 s (rss 1 kB, peak 2 kB)",
            ),
        ).copy(running = true)
        assertEquals(4, joining.lifted)
        assertEquals(1, joining.joined)
        assertEquals("Joining the parts, 2 of 3, about 1 min left", provingWords(joining))
        val done = progressOf(
            again + listOf(
                "native-claim: succinct receipt, 2 of 4 segments proved this run, in 400.0 s (rss 1 kB); written work/receipt",
                "== exit 0",
            ),
        )
        assertTrue(done.done)
        assertNull(done.error)
        assertEquals(1f, done.share)
    }

    @Test
    fun a_proof_that_fails_says_why() {
        val p = progressOf(listOf("== run 1 po2 18, 8 cpus", "Error: no packed guest at guests/native-claim.bin", "== exit 1"))
        assertEquals("no packed guest at guests/native-claim.bin", p.error)
        assertEquals("Waiting to start", provingWords(Proving()))
        assertEquals("Reading what to prove", provingWords(Proving(running = true)))
    }

    @Test
    fun a_phone_with_less_memory_proves_smaller_segments() {
        assertEquals(18, po2For(8L shl 30))
        assertEquals(17, po2For(5L shl 30))
        // The emulator's 2.9 GB.
        assertEquals(16, po2For(3_047_932L * 1024))
        assertEquals("under a minute", lasting(20))
        assertEquals("1 hour", lasting(3_600))
        assertEquals("30 hours", lasting(30 * 3_600))
    }
}
