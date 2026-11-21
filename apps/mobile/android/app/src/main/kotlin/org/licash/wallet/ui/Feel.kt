package org.licash.wallet.ui

import android.annotation.SuppressLint
import android.view.HapticFeedbackConstants
import android.view.View
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.platform.LocalView

/*
 * What the app feels like: a short haptic for an outcome, done or failed,
 * and a tick for a switch. Never on every tap, and never alone: the screen
 * says the same in words. Through the view, so the system's setting for
 * touch feedback is kept, and no permission is asked.
 */

/** What is felt. */
enum class Touch { DONE, FAILED, ON, OFF }

/**
 * The view's haptic constant for `touch` on a phone of Android `sdk`: the
 * confirm and reject effects from 30 on, the switch's own from 34, and
 * what the older ones have before, a short and a long press.
 */
@SuppressLint("InlinedApi")
internal fun haptic(touch: Touch, sdk: Int): Int = when (touch) {
    Touch.DONE -> if (sdk >= 30) HapticFeedbackConstants.CONFIRM else HapticFeedbackConstants.VIRTUAL_KEY
    Touch.FAILED -> if (sdk >= 30) HapticFeedbackConstants.REJECT else HapticFeedbackConstants.LONG_PRESS
    Touch.ON -> if (sdk >= 34) HapticFeedbackConstants.TOGGLE_ON else HapticFeedbackConstants.VIRTUAL_KEY
    Touch.OFF -> if (sdk >= 34) HapticFeedbackConstants.TOGGLE_OFF else HapticFeedbackConstants.VIRTUAL_KEY
}

/** Felt through `view`. */
class Feel internal constructor(private val view: View) {
    fun confirm() = feel(Touch.DONE)

    fun reject() = feel(Touch.FAILED)

    /** A switch turned `on`, or off. */
    fun toggle(on: Boolean) = feel(if (on) Touch.ON else Touch.OFF)

    private fun feel(touch: Touch) {
        view.performHapticFeedback(haptic(touch, android.os.Build.VERSION.SDK_INT))
    }
}

/** What the screen shown feels like. */
@Composable
fun rememberFeel(): Feel {
    val view = LocalView.current
    return remember(view) { Feel(view) }
}
