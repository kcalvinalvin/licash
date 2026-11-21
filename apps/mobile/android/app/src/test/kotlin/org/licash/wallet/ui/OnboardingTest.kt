package org.licash.wallet.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.licash.core.Network
import org.licash.wallet.Ui

class OnboardingTest {
    @Test
    fun the_networks_show_once_asked_for_and_never_hide_one_chosen() {
        assertFalse(networksShown(asked = false, Ui()))
        assertTrue(networksShown(asked = true, Ui()))
        // A test network chosen stays in sight, so it is never made unseen.
        assertTrue(networksShown(asked = false, Ui(network = Network.MUTINYNET)))
        assertTrue(networksShown(asked = false, Ui(developer = true)))
    }

    @Test
    fun no_hub_on_bitcoin_says_how_to_try_it_and_nothing_elsewhere() {
        assertEquals(NONE_ON_BITCOIN, noneFound(Network.BITCOIN, none = true))
        // A wallet with a hub already, or on a test network: only "None found".
        assertNull(noneFound(Network.BITCOIN, none = false))
        assertNull(noneFound(Network.MUTINYNET, none = true))
        assertNull(noneFound(Network.SIGNET, none = true))
    }
}
