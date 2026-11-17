package org.licash.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.licash.core.Hub
import org.licash.core.Rate
import org.licash.core.Scan
import org.licash.core.TorStatus
import org.licash.wallet.ChainStatus
import org.licash.wallet.Ui

class SyncTest {
    private fun hub() = Hub(
        url = "https://hub.example", feeBase = 0uL, feePpm = 0uL, feeOutputMsat = 0uL, deposits = null, silent = false,
        mailbox = false,
        lightningPay = null, lockFee = Rate(0uL, 0uL), refundFee = Rate(0uL, 0uL), depositFee = null,
        lightning = null, contracts = null,
    )

    private fun scan(from: UInt, next: UInt, height: UInt, tweaks: Boolean = true) =
        Scan(from = from, next = next, height = height, tweaks = tweaks, found = 0u, foundValue = 0uL)

    private val caughtUp = ChainStatus(height = 900, peers = 2, synced = true, headers = 900, peerHeight = 900)

    @Test
    fun a_fraction_only_on_a_known_way() {
        assertEquals(0.5f, fraction(0, 50, 100)!!, 0.0001f)
        assertEquals(0.25f, fraction(100, 125, 200)!!, 0.0001f)
        assertNull(fraction(0, 10, 0))
        assertNull(fraction(0, 150, 100))
        assertNull(fraction(100, 50, 200))
    }

    @Test
    fun a_new_wallet_waits_for_each_step_before_it() {
        val ui = Ui(chain = ChainStatus(peers = 1, headers = 400, peerHeight = 800), hubs = listOf(hub()))
        val stages = stages(ui)
        assertEquals(listOf("Bitcoin nodes", "Block headers", "Block filters", "Your payments"), stages.map { it.name })
        assertEquals(
            listOf(Progress.DONE, Progress.DOING, Progress.WAITING, Progress.WAITING),
            stages.map { it.state },
        )
        assertEquals("Block 400 of 800 (50%)", stages[1].detail)
        assertEquals(0.5f, stages[1].fraction!!, 0.0001f)
        // Waiting: no fraction shown.
        assertNull(stages[2].fraction)
        assertTrue(catchingUp(ui))
    }

    @Test
    fun no_peer_height_no_fraction() {
        val ui = Ui(chain = ChainStatus(peers = 1, headers = 400), hubs = listOf(hub()))
        val headers = stages(ui)[1]
        assertEquals("Block 400", headers.detail)
        assertNull(headers.fraction)
    }

    @Test
    fun tor_comes_first_over_tor_and_says_why_it_is_stuck() {
        val blocked = TorStatus(percent = 10u.toUByte(), ready = false, summary = "connecting", blocked = "clock skew")
        val ui = Ui(tor = true, torStatus = blocked, chain = caughtUp, hubs = listOf(hub()))
        val tor = stages(ui).first()
        assertEquals("Tor", tor.name)
        assertEquals(Progress.DOING, tor.state)
        assertEquals("Stuck: clock skew", tor.detail)
        assertEquals(0.1f, tor.fraction!!, 0.0001f)
        assertTrue(stages(ui).drop(1).all { it.state == Progress.WAITING })
        assertEquals("Starting", stages(ui.copy(torStatus = null)).first().detail)
    }

    @Test
    fun payments_are_looked_for_from_the_birthday() {
        val ui = Ui(chain = caughtUp, hubs = listOf(hub()), scan = scan(from = 800u, next = 850u, height = 900u))
        val payments = stages(ui).last()
        assertEquals(Progress.DOING, payments.state)
        assertEquals("Block 850 of 900, 51 to go (49%)", payments.detail)
        assertEquals(50f / 101f, payments.fraction!!, 0.0001f)
    }

    @Test
    fun caught_up_is_up_to_date_and_says_when_without_tweaks() {
        val ui = Ui(chain = caughtUp, hubs = listOf(hub()), scan = scan(from = 800u, next = 901u, height = 900u))
        assertEquals("Up to date", stages(ui).last().detail)
        assertFalse(catchingUp(ui))
        val fallback = ui.copy(scan = scan(from = 800u, next = 901u, height = 900u, tweaks = false))
        assertEquals("Up to date, fallback addresses only", stages(fallback).last().detail)
    }

    @Test
    fun a_hub_behind_the_light_client_is_waited_for() {
        val ui = Ui(chain = caughtUp, hubs = listOf(hub()), scan = scan(from = 800u, next = 896u, height = 895u))
        val payments = stages(ui).last()
        assertEquals(Progress.DOING, payments.state)
        assertEquals("Waiting for the hub's index of block 896", payments.detail)
    }

    @Test
    fun without_a_hub_payments_wait_for_one() {
        assertEquals("Choose a hub first", stages(Ui(chain = caughtUp)).last().detail)
    }
}
