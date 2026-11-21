package org.licash.wallet.ui

import android.os.SystemClock
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
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
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import org.licash.wallet.Engine
import org.licash.wallet.Ui
import org.licash.wallet.digits
import org.licash.wallet.explain

/** Where a step of catching up with Bitcoin stands. */
enum class Progress { DONE, DOING, WAITING }

/**
 * A step of catching up with Bitcoin: what it is, where it stands, what it
 * is doing in a few words, and how far, 0 to 1, only when what it counts
 * to is known.
 */
data class Stage(val name: String, val state: Progress, val detail: String, val fraction: Float? = null) {
    /** Whether the time it takes is shown: a step with no progress to count, that can take minutes. */
    val timed: Boolean get() = state == Progress.DOING && detail == PREPARING
}

/** The pinned headers downloaded, checked and stored before the light client starts: no progress told. */
internal const val PREPARING = "Preparing, this can take a few minutes"

/** How far `done` is of the way from `from` to `to`, if that way is known and `done` is on it. */
internal fun fraction(from: Long, done: Long, to: Long): Float? =
    if (to > from && done in from..to) (done - from).toFloat() / (to - from) else null

private fun shown(f: Float?): String = f?.let { " (${(it * 100).toInt()}%)" } ?: ""

private fun block(n: Long): String = digits(n.toULong())

/**
 * The steps of catching up with Bitcoin for `ui`, in their order: Tor if
 * on, the download of the headers this release pins while under way, the
 * nodes, the block headers, the block filters, then the wallet's
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
        add(TOR, ready, doing, "Connected", tor?.let { it.percent.toFloat() / 100f })
    }
    val chain = ui.chain
    if (chain.importing >= 0) {
        val f = chain.importing / 100f
        // Downloaded, the client checks and stores them: minutes, uncounted.
        if (chain.importing >= 100) add(HEADERS_DOWNLOAD, false, PREPARING, "")
        else add(HEADERS_DOWNLOAD, false, "Downloading" + shown(f), "", f)
    }
    val nodes = "${chain.peers} " + if (chain.peers == 1L) "node" else "nodes"
    add(NODES, chain.peers > 0, "Looking for nodes", nodes)
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
        add(PAYMENTS, false, "Choose a hub first", "")
        return out
    }
    val scan = ui.scan
    if (scan == null) {
        add(PAYMENTS, false, "Starting", "")
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
    add(PAYMENTS, looked && !lagging, doing, finished, fraction(from, next, reachable + 1).takeIf { !looked })
    return out
}

/** Whether a step is under way, the wallet catching up. */
fun catchingUp(ui: Ui): Boolean = stages(ui).any { it.state != Progress.DONE }

internal const val TOR = "Tor"
internal const val HEADERS_DOWNLOAD = "Block headers download"
internal const val NODES = "Bitcoin nodes"
internal const val PAYMENTS = "Your payments"

/**
 * A step under way in a few plain words: in advanced mode, its name and
 * where it is, block numbers and all.
 */
internal fun said(stage: Stage, developer: Boolean): String = when {
    developer -> "${stage.name}: ${stage.detail}"
    stage.state == Progress.WAITING -> "Waiting for Tor"
    stage.name == TOR -> stage.detail
    stage.name == HEADERS_DOWNLOAD -> if (stage.timed) PREPARING else "Downloading block headers" + shown(stage.fraction)
    stage.name == NODES -> "Looking for Bitcoin nodes"
    stage.name == PAYMENTS && stage.detail.startsWith("Waiting") -> "Waiting for your hub to catch up"
    stage.name == PAYMENTS -> "Looking for your payments" + shown(stage.fraction)
    else -> "Catching up with Bitcoin" + shown(stage.fraction)
}

/** How a connection stands, from fine to failing. */
enum class Reach { UP, GOING, DOWN }

/** A connection the app needs, and how it stands, in a few words. */
data class Link(val name: String, val reach: Reach, val detail: String)

/**
 * The app's connections: Tor if on, then Bitcoin, up once caught up (a
 * wallet with no hub yet does not look for payments, and is not behind).
 */
fun links(ui: Ui): List<Link> {
    val out = mutableListOf<Link>()
    val tor = ui.torStatus
    if (ui.tor) {
        out += when {
            tor?.ready == true -> Link(TOR, Reach.UP, "Connected")
            tor?.blocked != null -> Link(TOR, Reach.DOWN, "Stuck: ${tor.blocked}")
            else -> Link(TOR, Reach.GOING, "Connecting" + (tor?.let { " (${it.percent}%)" } ?: ""))
        }
    }
    val step = stages(ui).firstOrNull {
        it.state != Progress.DONE && it.name != TOR && !(it.name == PAYMENTS && ui.hubs.isEmpty())
    }
    val chain = ui.chain
    val up = "Up to date" + if (ui.developer) ", block ${block(chain.height.toLong())}, ${chain.peers} nodes" else ""
    out += Link(BITCOIN, if (step == null) Reach.UP else Reach.GOING, step?.let { said(it, ui.developer) } ?: up)
    for (hub in ui.hubs) {
        out += when (ui.hubsReached[hub.url]) {
            true -> Link(name(hub.url), Reach.UP, "Reached")
            false -> Link(name(hub.url), Reach.DOWN, "Not answering")
            null -> Link(name(hub.url), Reach.GOING, "Not asked yet")
        }
    }
    // Only a wallet with a mailbox looks in one.
    if (ui.hubs.any { it.mailbox }) {
        out += when (ui.relaysReached) {
            true -> Link(RELAYS, Reach.UP, "Reached")
            false -> Link(RELAYS, Reach.DOWN, "None reached: payments to you wait there until one is")
            null -> Link(RELAYS, Reach.GOING, "Not looked in yet")
        }
    }
    return out
}

/** Bitcoin, as a connection is named. */
internal const val BITCOIN = "Bitcoin"

/** The mailbox's relays, as a connection is named. */
internal const val RELAYS = "Mailbox relays"

/** How long Tor may not get through before turning it off is offered: a first start takes a minute or two. */
internal const val TOR_PATIENCE_MS = 180_000L

/** Whether Tor keeps the app from Bitcoin now: on, and not connected or no node reached through it. */
internal fun torInTheWay(ui: Ui): Boolean = ui.tor && (ui.torStatus?.ready != true || ui.chain.peers == 0L)

/**
 * Whether turning Tor off is offered, it in the way for `stuckMs` (null if
 * not): at once if Tor says it is stuck, else after [TOR_PATIENCE_MS].
 */
internal fun offerTorOff(ui: Ui, stuckMs: Long?): Boolean =
    torInTheWay(ui) && (ui.torStatus?.blocked != null || (stuckMs ?: 0L) >= TOR_PATIENCE_MS)

/** What turning Tor off means, said beside the button. */
internal const val TOR_OFF_MEANS = "Tor is not getting through. Without it the app connects directly: " +
    "faster, but hubs, relays and Bitcoin nodes see your IP address. You can turn it on again in Settings."

/** The app's state kept fresh every two seconds while shown. */
@Composable
private fun KeptFresh(engine: Engine) {
    LaunchedEffect(Unit) {
        while (true) {
            engine.live()
            delay(2_000)
        }
    }
}

/**
 * The steps of catching up, each with its progress, kept fresh every two
 * seconds while shown.
 */
@Composable
fun SyncStages(engine: Engine, ui: Ui) {
    KeptFresh(engine)
    val preparing = rememberWait(ui.chain.importing >= 100)
    Column(Modifier.fillMaxWidth().testTag("sync-stages")) {
        for (stage in stages(ui)) StageRow(stage, preparing)
    }
}

/**
 * Catching up, on Home: each step in advanced mode, else only the one
 * under way, in plain words, and how far.
 */
@Composable
fun CatchingUp(engine: Engine, ui: Ui) {
    if (ui.developer) {
        SyncStages(engine, ui)
        return
    }
    KeptFresh(engine)
    val preparing = rememberWait(ui.chain.importing >= 100)
    val step = stages(ui).firstOrNull { it.state != Progress.DONE } ?: return
    Column(Modifier.fillMaxWidth().padding(vertical = 8.dp).testTag("catching-up")) {
        Text(said(step, developer = false), style = MaterialTheme.typography.bodyLarge)
        if (step.timed) Waited(preparing)
        Spacer(Modifier.height(6.dp))
        val f = step.fraction
        if (f != null) LinearProgressIndicator(progress = { f }, modifier = Modifier.fillMaxWidth())
        else LinearProgressIndicator(Modifier.fillMaxWidth())
    }
}

/**
 * A dot, by the network, for how the app's connections stand: a tap
 * shows each, and offers turning Tor off once it keeps the app from
 * Bitcoin.
 */
@Composable
fun Connections(engine: Engine, ui: Ui) {
    val links = links(ui)
    val worst = links.maxOf { it.reach }
    // Counted while closed too: how long Tor has been in the way.
    val stuck = rememberWait(torInTheWay(ui))
    var open by remember { mutableStateOf(false) }
    val described = "Connections: " + when (worst) {
        Reach.UP -> "up to date"
        Reach.GOING -> "connecting"
        Reach.DOWN -> "a problem"
    }
    Box(
        Modifier.size(32.dp).clip(CircleShape).clickable { open = true }
            .semantics { contentDescription = described }.testTag("connections"),
        contentAlignment = Alignment.Center,
    ) {
        Box(Modifier.size(10.dp).background(tint(worst), CircleShape))
    }
    if (!open) return
    val scope = rememberCoroutineScope()
    var error by remember { mutableStateOf<String?>(null) }
    Sheet("Connections", dismiss = { open = false }) {
        KeptFresh(engine)
        for (link in links) LinkRow(link)
        if (ui.developer && links.first { it.name == BITCOIN }.reach != Reach.UP) SyncStages(engine, ui)
        val since = stuck.since
        if (offerTorOff(ui, since?.let { SystemClock.elapsedRealtime() - it })) {
            Spacer(Modifier.height(8.dp))
            Say(TOR_OFF_MEANS)
            OutlinedButton(
                onClick = { scope.launch { engine.setTor(false).onFailure { error = explain(it) } } },
                modifier = Modifier.fillMaxWidth().testTag("tor-off"),
            ) { Text("Turn Tor off") }
        }
        error?.let { Problem(it) }
    }
}

@Composable
private fun tint(reach: Reach): Color = when (reach) {
    Reach.UP -> MaterialTheme.colorScheme.primary
    Reach.GOING -> MaterialTheme.colorScheme.outline
    Reach.DOWN -> MaterialTheme.colorScheme.error
}

@Composable
private fun LinkRow(link: Link) {
    Row(Modifier.fillMaxWidth().padding(vertical = 8.dp), verticalAlignment = Alignment.CenterVertically) {
        Box(Modifier.size(10.dp).background(tint(link.reach), CircleShape))
        Spacer(Modifier.width(12.dp))
        Column(Modifier.weight(1f)) {
            Text(link.name, style = MaterialTheme.typography.bodyLarge)
            Text(
                link.detail,
                modifier = Modifier.testTag("link ${link.name}"),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

@Composable
private fun StageRow(stage: Stage, preparing: Wait) {
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
            if (stage.timed) Waited(preparing)
            stage.fraction?.let {
                Spacer(Modifier.height(6.dp))
                LinearProgressIndicator(progress = { it }, modifier = Modifier.fillMaxWidth())
            }
        }
    }
}
