package org.licash.wallet.ui

import android.os.SystemClock
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.compositionLocalOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.style.TextAlign
import kotlinx.coroutines.delay

/**
 * A wait the user sees through: since when it lasts, while it does, and
 * how long the last one took once over. Times are `SystemClock` ms.
 */
class Wait {
    var since: Long? by mutableStateOf(null)
        private set
    var took: Long? by mutableStateOf(null)
        private set

    /** The wait starts, or ends, at `now`: told each time it may have. */
    fun turn(waiting: Boolean, now: Long) {
        val began = since
        if (waiting && began == null) {
            since = now
            took = null
        } else if (!waiting && began != null) {
            took = now - began
            since = null
        }
    }

    /** What [Waited] says at `now`: the seconds so far, how long it took, or nothing. */
    fun says(now: Long): String? =
        since?.let { ticking(((now - it) / 1000).toULong()) } ?: took?.takeIf { it >= NOTICED_MS }?.let { "Took ${lasted(it)}" }

    private companion object {
        /** A wait shorter goes unsaid once over: nothing was waited on. */
        const val NOTICED_MS = 100L
    }
}

/** `ms` as said once over: `0.4 s` and `9.9 s`, then as a clock ticks them. */
fun lasted(ms: Long): String =
    if (ms < 10_000) "${ms / 1000}.${ms % 1000 / 100} s" else ticking((ms / 1000).toULong())

/** The wait of a screen, `waiting` or not now. */
@Composable
fun rememberWait(waiting: Boolean): Wait {
    val wait = remember { Wait() }
    // Once each time `waiting` changes, before what reads it composes.
    remember(waiting) { wait.turn(waiting, SystemClock.elapsedRealtime()) }
    return wait
}

/** The wait of the screen shown, for the parts all its steps share. */
val LocalWait = compositionLocalOf<Wait?> { null }

/**
 * Softly, under what is waited on, the seconds it lasts, ticking; once
 * over, how long it took. Nothing before any wait.
 */
@Composable
fun Waited(wait: Wait? = LocalWait.current, modifier: Modifier = Modifier, center: Boolean = false) {
    wait ?: return
    val since = wait.since
    val now by produceState(SystemClock.elapsedRealtime(), since) {
        while (since != null) {
            delay(1000 - (SystemClock.elapsedRealtime() - since) % 1000)
            value = SystemClock.elapsedRealtime()
        }
    }
    val says = wait.says(maxOf(now, since ?: 0L)) ?: return
    Text(
        says,
        modifier = modifier.testTag("elapsed"),
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        textAlign = if (center) TextAlign.Center else null,
    )
}
