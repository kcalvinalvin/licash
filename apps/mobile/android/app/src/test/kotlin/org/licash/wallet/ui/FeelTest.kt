package org.licash.wallet.ui

import android.view.HapticFeedbackConstants
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Test

class FeelTest {
    @Test
    fun an_outcome_is_confirmed_or_rejected_from_android_11_and_a_press_before() {
        assertEquals(HapticFeedbackConstants.CONFIRM, haptic(Touch.DONE, 30))
        assertEquals(HapticFeedbackConstants.REJECT, haptic(Touch.FAILED, 30))
        assertEquals(HapticFeedbackConstants.CONFIRM, haptic(Touch.DONE, 36))
        // The app's oldest phones (Android 8): a short tick for done, a long press for failed.
        assertEquals(HapticFeedbackConstants.VIRTUAL_KEY, haptic(Touch.DONE, 26))
        assertEquals(HapticFeedbackConstants.LONG_PRESS, haptic(Touch.FAILED, 29))
    }

    @Test
    fun a_switch_has_its_own_tick_from_android_14() {
        assertEquals(HapticFeedbackConstants.TOGGLE_ON, haptic(Touch.ON, 34))
        assertEquals(HapticFeedbackConstants.TOGGLE_OFF, haptic(Touch.OFF, 34))
        assertEquals(HapticFeedbackConstants.VIRTUAL_KEY, haptic(Touch.ON, 33))
        assertEquals(HapticFeedbackConstants.VIRTUAL_KEY, haptic(Touch.OFF, 26))
    }

    @Test
    fun done_and_failed_never_feel_alike() {
        for (sdk in listOf(26, 29, 30, 34, 36)) {
            assertNotEquals("at $sdk", haptic(Touch.DONE, sdk), haptic(Touch.FAILED, sdk))
        }
    }
}
