package org.licash.wallet.ui

import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.delay
import org.licash.wallet.Engine
import org.licash.wallet.Ui
import org.licash.wallet.digits

/** Where a step of catching up with Bitcoin stands. */
enum class Progress { DONE, DOING, WAITING }

/**
 * A step of catching up with Bitcoin: what it is, where it stands, what it
 * is doing in a few words, and how far, 0 to 1, only when what it counts
 * to is known.
 */
data class Stage(val name: String, val state: Progress, val detail: String, val fraction: Float? = null)

/** How far `done` is of the way from `from` to `to`, if that way is known and `done` is on it. */
internal fun fraction(from: Long, done: Long, to: Long): Float? =
    if (to > from && done in from..to) (done - from).toFloat() / (to - from) else null

private fun shown(f: Float?): String = f?.let { " (${(it * 100).toInt()}%)" } ?: ""

private fun block(n: Long): String = digits(n.toULong())

/**
 * The steps of catching up with Bitcoin for `ui`, in their order: Tor if
 * on, the nodes, the block headers, the block filters, then the wallet's
 * payments. A step waits for the one before it.
 */
fun stages(ui: Ui): List<Stage> {
    val out = mutableListOf<Stage>()
    var before = true
    fun add(name: String, done: Boolean, doing: String, finished: String, fraction: Float? = null) {
        val state = when {
            !before -> Progress.WAITING
            done -> Progress.DONE
            else -> Progress.DOING
        }
        out += Stage(name, state, if (state == Progress.DONE) finished else doing, fraction.takeIf { state == Progress.DOING })
        before = before && done
    }
    val tor = ui.torStatus
    if (ui.tor) {
        val ready = tor?.ready == true
        val doing = when {
            tor == null -> "Starting"
            tor.blocked != null -> "Stuck: ${tor.blocked}"
            else -> tor.summary
        }
        add("Tor", ready, doing, "Connected", tor?.let { it.percent.toFloat() / 100f })
    }
    val chain = ui.chain
    val nodes = "${chain.peers} " + if (chain.peers == 1L) "node" else "nodes"
    add("Bitcoin nodes", chain.peers > 0, "Looking for nodes", nodes)
    val target = chain.peerHeight.toLong().takeIf { it > 0 }
    val headers = chain.headers.toLong()
    val headed = chain.synced || (target != null && headers >= target)
    add(
        "Block headers",
        headed,
        "Block ${block(headers)}" + (target?.let { " of ${block(it)}" } ?: "") +
            shown(target?.let { fraction(0, headers, it) }),
        "Block ${block(headers)}",
        target?.let { fraction(0, headers, it) },
    )
    val filters = chain.height.toLong()
    add(
        "Block filters",
        chain.synced,
        "Block ${block(filters)} of ${block(headers)}" + shown(fraction(0, filters, headers)),
        "Block ${block(filters)}",
        fraction(0, filters, headers),
    )
    if (ui.hubs.isEmpty()) {
        add("Your payments", false, "Choose a hub first", "")
        return out
    }
    val scan = ui.scan
    if (scan == null) {
        add("Your payments", false, "Starting", "")
        return out
    }
    val next = scan.next.toLong()
    val reachable = scan.height.toLong()
    val from = scan.from.toLong().takeIf { it in 1..next } ?: next
    // The hubs index the blocks after the light client has them.
    val lagging = reachable < filters
    val looked = next > reachable
    val doing = when {
        looked && lagging -> "Waiting for the hub's index of block ${block(reachable + 1)}"
        else -> "Block ${block(next)} of ${block(reachable)}, ${block(reachable - next + 1)} to go" +
            shown(fraction(from, next, reachable + 1))
    }
    val finished = "Up to date" + if (scan.tweaks) "" else ", fallback addresses only"
    add("Your payments", looked && !lagging, doing, finished, fraction(from, next, reachable + 1).takeIf { !looked })
    return out
}

/** Whether a step is under way, the wallet catching up. */
fun catchingUp(ui: Ui): Boolean = stages(ui).any { it.state != Progress.DONE }

/**
 * The steps of catching up, each with its progress, kept fresh every two
 * seconds while shown.
 */
@Composable
fun SyncStages(engine: Engine, ui: Ui) {
    LaunchedEffect(Unit) {
        while (true) {
            engine.live()
            delay(2_000)
        }
    }
    Column(Modifier.fillMaxWidth().testTag("sync-stages")) {
        for (stage in stages(ui)) StageRow(stage)
    }
}

@Composable
private fun StageRow(stage: Stage) {
    val colors = MaterialTheme.colorScheme
    Row(Modifier.fillMaxWidth().padding(vertical = 8.dp), verticalAlignment = Alignment.Top) {
        when (stage.state) {
            Progress.DONE -> Icon(Icons.Filled.CheckCircle, null, Modifier.size(20.dp), tint = colors.primary)
            Progress.DOING -> CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp)
            Progress.WAITING -> Box(Modifier.padding(2.dp).size(16.dp).border(2.dp, colors.outline, CircleShape))
        }
        Spacer(Modifier.width(12.dp))
        Column(Modifier.weight(1f)) {
            Text(stage.name, style = MaterialTheme.typography.bodyLarge)
            if (stage.detail.isNotEmpty()) {
                Text(
                    stage.detail,
                    modifier = Modifier.testTag("stage ${stage.name}"),
                    style = MaterialTheme.typography.bodySmall,
                    color = colors.onSurfaceVariant,
                )
            }
            stage.fraction?.let {
                Spacer(Modifier.height(6.dp))
                LinearProgressIndicator(progress = { it }, modifier = Modifier.fillMaxWidth())
            }
        }
    }
}
