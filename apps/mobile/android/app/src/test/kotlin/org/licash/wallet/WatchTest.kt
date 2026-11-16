package org.licash.wallet

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.licash.core.Activity
import org.licash.core.Details
import org.licash.core.Kind
import org.licash.core.LockStep
import org.licash.core.Onchain
import org.licash.core.PayoutSteps
import org.licash.core.Recovery
import org.licash.core.RecoveryStage
import org.licash.core.State

class WatchTest {
    private val now = 1_800_000_000L

    private fun line(kind: Kind, state: State, id: String = "a", amount: ULong = 1_000uL, at: Long? = null) =
        Activity(id = id, kind = kind, state = state, amount = amount, fee = 0uL, detail = "the hub refused", at = at)

    private fun steps(coin: String? = "c0", claim: String? = null, claimBy: UInt? = 1_000u, tip: UInt = 900u) =
        PayoutSteps(
            lock = LockStep.LOCKED, lockExpiry = 1_100u, timeout = 1_010u, claimBy = claimBy, coin = coin,
            coinHeight = null, claim = claim, claimHeight = null, depth = 1u, tip = tip,
        )

    private fun payout(steps: PayoutSteps) = Details.Payout(
        hub = "https://hub.example", provider = "https://swaps.example", address = "bc1q", coin = steps.coin,
        claim = steps.claim, failure = null, progress = steps, providerFee = 0u, claimFee = 0u, lockFee = 0u,
    )

    private fun deposit(paid: Boolean) = Details.Moved(
        hub = "https://hub.example", hash = "h", pays = null, address = "bc1q",
        payment = if (paid) Onchain(txid = "t", fee = null, height = null, confirmations = 0u) else null,
        depth = null, fundBy = null, timeout = null, refund = null,
    )

    @Test
    fun nothing_under_way_is_not_watched() {
        val ui = Ui(history = listOf(line(Kind.SENT, State.DONE), line(Kind.RECEIVED, State.DONE, "b")))
        assertEquals(Watch.NONE, watchOf(ui, now))
    }

    @Test
    fun a_payment_under_way_is_watched_now() {
        for (kind in listOf(Kind.PAYOUT, Kind.SWAP, Kind.INVOICE, Kind.LIGHTNING, Kind.SENT, Kind.ONCHAIN)) {
            assertEquals(kind.name, Watch.NOW, watchOf(Ui(history = listOf(line(kind, State.PENDING))), now))
        }
    }

    @Test
    fun a_deposit_never_sent_is_looked_at_later_but_once_sent_or_unknown_now() {
        val moved = line(Kind.MOVED, State.PENDING)
        assertEquals(Watch.LATER, watchOf(Ui(history = listOf(moved), under = mapOf(moved.key to deposit(false))), now))
        assertEquals(Watch.NOW, watchOf(Ui(history = listOf(moved), under = mapOf(moved.key to deposit(true))), now))
        // Where it stands unread (past the first lines under way): perhaps sent.
        assertEquals(Watch.NOW, watchOf(Ui(history = listOf(moved)), now))
    }

    @Test
    fun a_payment_on_its_way_to_its_payee_is_looked_at_later() {
        assertEquals(Watch.LATER, watchOf(Ui(onTheirWay = 1u), now))
        assertEquals(Watch.NONE, watchOf(Ui(onTheirWay = 0u), now))
    }

    @Test
    fun a_contract_being_opened_is_watched_now() {
        assertEquals(Watch.NOW, watchOf(Ui(opening = "https://hub.example"), now))
    }

    @Test
    fun a_line_under_way_for_over_a_week_is_stuck_not_watched() {
        val stuck = line(Kind.PAYOUT, State.PENDING, at = now - STALE_SECS)
        assertEquals(Watch.NONE, watchOf(Ui(history = listOf(stuck)), now))
        val recent = line(Kind.PAYOUT, State.PENDING, at = now - STALE_SECS + 60)
        assertEquals(Watch.NOW, watchOf(Ui(history = listOf(recent)), now))
    }

    @Test
    fun a_contract_forced_closed_is_looked_at_later_until_over() {
        fun forced(stage: RecoveryStage) = Recovery(
            id = "c0", stage = stage, irrevocable = true, claimed = 3_000uL, notes = 7u, request = null,
            publication = null, timeout = null, sweep = null, nextAt = null, error = null,
        )
        for (stage in listOf(RecoveryStage.CLAIMED, RecoveryStage.REQUESTED, RecoveryStage.SWEEPING)) {
            assertEquals(stage.name, Watch.LATER, watchOf(Ui(recoveries = listOf(forced(stage))), now))
        }
        for (stage in listOf(RecoveryStage.DONE, RecoveryStage.ANSWERED, RecoveryStage.LOST)) {
            assertEquals(stage.name, Watch.NONE, watchOf(Ui(recoveries = listOf(forced(stage))), now))
        }
    }

    @Test
    fun a_proof_posted_keeps_the_app_at_work_until_paid() {
        fun forced(stage: RecoveryStage) = Recovery(
            id = "c0", stage = stage, irrevocable = true, claimed = 3_000uL, notes = 7u, request = null,
            publication = null, timeout = null, sweep = null, nextAt = null, error = null, deadline = 2_000u,
        )
        for (stage in listOf(RecoveryStage.ASSERTING, RecoveryStage.ASSERTED, RecoveryStage.CHALLENGED)) {
            assertEquals(stage.name, Watch.NOW, watchOf(Ui(recoveries = listOf(forced(stage))), now))
        }
        for (stage in listOf(RecoveryStage.PROVING, RecoveryStage.PAID)) {
            assertEquals(stage.name, Watch.LATER, watchOf(Ui(recoveries = listOf(forced(stage))), now))
        }
    }

    @Test
    fun a_lightning_invoice_asked_is_looked_for_later_for_a_day() {
        assertEquals(Watch.LATER, watchOf(Ui(asked = now - ASKED_SECS + 60), now))
        assertEquals(Watch.NONE, watchOf(Ui(asked = now - ASKED_SECS), now))
    }

    @Test
    fun an_invoice_paid_at_once_is_told_if_not_seen_before_in_this_run() {
        val paid = line(Kind.INVOICE, State.DONE, "i")
        val old = line(Kind.PAYOUT, State.DONE, "o")
        // Seen nothing yet since the app started: nothing new to tell.
        assertTrue(look(listOf(paid, old), emptySet(), null).finished.isEmpty())
        val look = look(listOf(paid, old), emptySet(), setOf(old.key))
        assertEquals(listOf(paid), look.finished)
        assertTrue(look(listOf(paid, old), emptySet(), setOf(old.key, paid.key)).finished.isEmpty())
        // Never looked before (an upgraded app): nothing.
        assertTrue(look(listOf(paid, old), null, setOf(old.key)).finished.isEmpty())
    }

    @Test
    fun never_looked_before_nothing_is_told_finished() {
        val history = listOf(line(Kind.PAYOUT, State.DONE, "old"), line(Kind.SWAP, State.PENDING, "now"))
        val look = look(history, null)
        assertTrue(look.finished.isEmpty())
        assertEquals(setOf("${Kind.SWAP} now"), look.under)
    }

    @Test
    fun a_line_under_way_when_last_looked_is_told_once_finished() {
        val before = look(listOf(line(Kind.PAYOUT, State.PENDING, "p"), line(Kind.SWAP, State.PENDING, "s")), null)
        val after = listOf(line(Kind.PAYOUT, State.DONE, "p"), line(Kind.SWAP, State.PENDING, "s"))
        val look = look(after, before.under)
        assertEquals(listOf("p"), look.finished.map { it.id })
        assertEquals(setOf("${Kind.SWAP} s"), look.under)
        // Not again.
        assertTrue(look(after, look.under).finished.isEmpty())
    }

    @Test
    fun lines_of_other_kinds_sharing_an_id_are_apart() {
        val before = setOf("${Kind.SWAP} x")
        val look = look(listOf(line(Kind.PAYOUT, State.DONE, "x"), line(Kind.SWAP, State.PENDING, "x")), before)
        assertTrue(look.finished.isEmpty())
    }

    @Test
    fun finished_lines_are_told_in_words_failures_with_why() {
        assertEquals("Sent 21,852 sat on Bitcoin", news(line(Kind.PAYOUT, State.DONE, amount = 21_852uL)))
        assertEquals(
            "Moving 1,000 sat between hubs failed: open Licash for why",
            news(line(Kind.SWAP, State.FAILED)),
        )
        assertNull(news(line(Kind.PAYOUT, State.PENDING)))
        // A coin moving in shows as its deposit, told of instead.
        assertNull(news(line(Kind.ONCHAIN, State.DONE)))
    }

    @Test
    fun payout_deadlines_are_read_beyond_the_ordinary_progress_limit() {
        val ordinary = (1..25).map { line(Kind.SWAP, State.PENDING, "s$it") }
        val p = line(Kind.PAYOUT, State.PENDING, "p")
        val done = line(Kind.PAYOUT, State.DONE, "done")
        val read = detailsToRead(ordinary + p + done)
        assertEquals(ordinary.take(20) + p, read)
        val under = read.filter { it.kind == Kind.PAYOUT }.associate { it.key to payout(steps(tip = 999u)) }
        assertEquals(mapOf(p.key to 1_000u), nearing(Ui(history = ordinary + p + done, under = under)))
    }

    @Test
    fun a_payout_is_warned_of_only_with_the_providers_coin_unclaimed_near_its_deadline() {
        val p = line(Kind.PAYOUT, State.PENDING, "p")
        fun near(steps: PayoutSteps, height: Int = 0) =
            nearing(Ui(history = listOf(p), under = mapOf(p.key to payout(steps)), chain = ChainStatus(height = height)))
        assertEquals(mapOf(p.key to 1_000u), near(steps(tip = 1_000u - WARN_BLOCKS)))
        assertEquals(emptyMap<String, UInt>(), near(steps(tip = 1_000u - WARN_BLOCKS - 1u)))
        // The light client further than the wallet last looked.
        assertEquals(mapOf(p.key to 1_000u), near(steps(tip = 900u), height = 995))
        assertEquals(emptyMap<String, UInt>(), near(steps(coin = null, tip = 999u)))
        assertEquals(emptyMap<String, UInt>(), near(steps(claim = "c1", tip = 999u)))
        assertEquals(emptyMap<String, UInt>(), near(steps(claimBy = null, tip = 999u)))
    }
}
