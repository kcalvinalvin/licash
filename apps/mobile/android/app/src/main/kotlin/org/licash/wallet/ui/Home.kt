package org.licash.wallet.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectVerticalDragGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.KeyboardArrowUp
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material.icons.filled.Warning
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.input.nestedscroll.NestedScrollConnection
import androidx.compose.ui.input.nestedscroll.NestedScrollSource
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.Velocity
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import org.licash.core.Activity
import org.licash.core.Holding
import org.licash.wallet.Engine
import org.licash.wallet.behind
import org.licash.wallet.Ui
import org.licash.wallet.digits
import org.licash.wallet.sat
import org.licash.wallet.total

/**
 * Home, at a glance: what the wallet holds, how much of it is protected,
 * and what is going on, if anything; to receive and to send. The rest is a
 * tap away: where the money is behind the balance, the history behind
 * Activity, or a swipe up.
 */
@Composable
fun Home(
    engine: Engine,
    ui: Ui,
    receive: () -> Unit,
    send: () -> Unit,
    settings: () -> Unit,
    money: (at: String?) -> Unit,
    history: () -> Unit,
    open: (Activity) -> Unit,
) {
    // A swipe up opens the history, wherever it starts: what the middle,
    // which scrolls, leaves of one, and one over the rest.
    val up by rememberUpdatedState(history)
    val far = with(LocalDensity.current) { 64.dp.toPx() }
    val pull = remember(far) {
        object : NestedScrollConnection {
            var dragged = 0f

            override fun onPostScroll(consumed: Offset, available: Offset, source: NestedScrollSource): Offset {
                if (source == NestedScrollSource.UserInput) dragged += available.y
                return Offset.Zero
            }

            override suspend fun onPreFling(available: Velocity): Velocity {
                if (dragged < -far) up()
                dragged = 0f
                return Velocity.Zero
            }
        }
    }
    Column(
        Modifier.fillMaxSize().padding(horizontal = 24.dp).nestedScroll(pull).pointerInput(far) {
            var dragged = 0f
            detectVerticalDragGestures(
                onDragStart = { dragged = 0f },
                onDragEnd = { if (dragged < -far) up() },
            ) { _, by -> dragged += by }
        },
    ) {
        Row(Modifier.fillMaxWidth().padding(top = 8.dp), verticalAlignment = Alignment.CenterVertically) {
            Wordmark()
            Spacer(Modifier.width(10.dp))
            NetworkChip(ui.network)
            Connections(engine, ui)
            Spacer(Modifier.weight(1f))
            IconButton(onClick = settings) { Icon(Icons.Default.Settings, contentDescription = "Settings") }
        }
        // In the middle of what is left, scrolling on short screens and with
        // large fonts; pulled down, it looks for payments now.
        PullToLook(engine, Modifier.fillMaxWidth().weight(1f)) {
            Column(
                Modifier.fillMaxSize().verticalScroll(rememberScrollState()),
                horizontalAlignment = Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.Center,
            ) {
                // Nothing works without a hub: choosing one comes first.
                if (ui.hubs.isEmpty()) {
                    ChooseHub(engine, ui)
                } else {
                    Total(ui) { money(null) }
                    Going(ui, money, history, open)
                    Offer(engine, ui)
                    Waiting(ui, money)
                    Unprotected(engine, ui, money)
                    // A new wallet's first catching up: each step, as it goes.
                    if (firstCatchingUp(ui)) {
                        Spacer(Modifier.height(16.dp))
                        Column(
                            Modifier.fillMaxWidth().clip(RoundedCornerShape(16.dp))
                                .background(MaterialTheme.colorScheme.surfaceContainer).padding(horizontal = 16.dp, vertical = 8.dp),
                        ) {
                            CatchingUp(engine, ui)
                            Text(
                                "You can leave the app: it keeps catching up, its progress in a notification.",
                                modifier = Modifier.padding(bottom = 8.dp),
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                    }
                }
            }
        }
        if (ui.hubs.isNotEmpty()) {
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                BigButton("Receive", Modifier.weight(1f).testTag("receive"), onClick = receive)
                BigButton("Send", Modifier.weight(1f).testTag("send"), onClick = send)
            }
            Column(
                Modifier.fillMaxWidth().padding(top = 8.dp, bottom = 8.dp).clip(RoundedCornerShape(16.dp))
                    .clickable(onClick = history).padding(vertical = 8.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                Icon(
                    Icons.Default.KeyboardArrowUp,
                    contentDescription = null,
                    Modifier.size(20.dp),
                    tint = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Text(
                    "Activity",
                    modifier = Modifier.testTag("activity"),
                    style = MaterialTheme.typography.labelLarge,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
    }
}

/** What the wallet holds, and how much of it is protected: both open where it is. */
@Composable
private fun Total(ui: Ui, open: () -> Unit) {
    val colors = MaterialTheme.colorScheme
    val figure = digits(ui.balance.total)
    Row(
        Modifier.clip(RoundedCornerShape(16.dp)).clickable(onClick = open).padding(horizontal = 8.dp),
        verticalAlignment = Alignment.Bottom,
    ) {
        // Smaller once long, to stay on one line.
        val size = when {
            figure.length <= 9 -> 56
            figure.length <= 11 -> 46
            else -> 36
        }
        Text(
            figure,
            modifier = Modifier.testTag("balance"),
            style = Figures.copy(fontSize = size.sp),
            maxLines = 1,
        )
        Text(
            " sat",
            modifier = Modifier.padding(bottom = 8.dp),
            style = MaterialTheme.typography.titleMedium,
            color = colors.onSurfaceVariant,
        )
    }
    val held = homeProtection(ui.balance, ui.opening != null) ?: return
    held.label?.let { label ->
        Spacer(Modifier.height(12.dp))
        Pill(
            icon = if (held.shield == Shield.NONE) Glyphs.shieldOff else Glyphs.shield,
            text = label,
            // Calm, whichever: what is not protected is said, not shouted.
            tint = if (held.shield == Shield.ALL) colors.primary else colors.onSurfaceVariant,
            tag = "protection",
            onClick = open,
        )
    }
}

/** What is going on, in a line: see [glance]. */
@Composable
private fun Going(ui: Ui, money: (String?) -> Unit, history: () -> Unit, open: (Activity) -> Unit) {
    val going = glance(ui, System.currentTimeMillis() / 1000) ?: return
    var why by remember { mutableStateOf<String?>(null) }
    Spacer(Modifier.height(8.dp))
    Pill(
        icon = Icons.Default.Warning,
        text = going.text,
        busy = going !is Glance.Behind,
        tag = "status",
        onClick = when (going) {
            is Glance.One -> ({ open(going.activity) })
            is Glance.Many -> history
            is Glance.Protecting -> ({ money(going.hub) })
            is Glance.Behind -> ({ why = going.reason })
        },
    )
    why?.let { Sheet("Not up to date", dismiss = { why = null }) { Say(it) } }
}

/**
 * The protection offered for what was paid on Bitcoin, to pay with it
 * once protected: until accepted, declined or being opened.
 */
@Composable
private fun Offer(engine: Engine, ui: Ui) {
    val q = ui.offer?.takeIf { !ui.offerDeclined && ui.opening == null } ?: return
    val coin = q.coins.firstOrNull() ?: return
    var choosing by remember(q) { mutableStateOf(false) }
    Spacer(Modifier.height(8.dp))
    Pill(
        icon = Glyphs.shield,
        // Already safe on Bitcoin: what it lacks is being spendable here.
        text = "Pay with your ${sat(coin.value)} on Bitcoin",
        tint = MaterialTheme.colorScheme.primary,
        tag = "offer",
        onClick = { choosing = true },
    )
    if (choosing) OfferSheet(engine, q, !behind(ui)) { choosing = false }
}

/**
 * Bitcoin paid to the wallet that no protection is offered for, nor any is
 * being set up: said, so it is not left to be found, and opening where it
 * is held and what to do with it.
 */
@Composable
private fun Waiting(ui: Ui, money: (String?) -> Unit) {
    val coin = waiting(ui.balance, offered = ui.offer != null, opening = ui.opening != null) ?: return
    Spacer(Modifier.height(8.dp))
    Pill(
        icon = Glyphs.bitcoin,
        text = "${sat(coin)} on Bitcoin, not in your balance",
        tag = "waiting",
        onClick = { money(ON_BITCOIN) },
    )
}

/**
 * A hub holding more than its contracts protect, by at least its smallest
 * contract: said until dismissed on its page, which offers protecting it
 * or cashing part out.
 */
@Composable
private fun Unprotected(engine: Engine, ui: Ui, money: (String?) -> Unit) {
    var holdings by remember { mutableStateOf<List<Holding>>(emptyList()) }
    LaunchedEffect(ui.balance, ui.contracts, ui.hubs) { engine.holdings().onSuccess { holdings = it } }
    val (hub, missing) = holdings.firstNotNullOfOrNull { h ->
        val cover = hubCover(h.hub, h, ui.contracts, emptyList())
        val terms = ui.hubs.find { sameHub(it.url, h.hub) }
        (h.hub to cover.missing).takeIf { suggestProtecting(cover, terms, engine.prefs.unprotectedDismissed(h.hub)) }
    } ?: return
    Spacer(Modifier.height(8.dp))
    Pill(
        icon = Glyphs.shieldOff,
        text = "${sat(missing)} at ${name(hub)} not protected",
        tag = "unprotected",
        onClick = { money(hub) },
    )
}

/**
 * No hub yet: those announced to pick from, looked for at once, or one by
 * the address its operator gave. What a hub is, once asked.
 */
@Composable
private fun ChooseHub(engine: Engine, ui: Ui) {
    Column(Modifier.fillMaxWidth()) {
        Row(Modifier.align(Alignment.CenterHorizontally), verticalAlignment = Alignment.CenterVertically) {
            Text("Choose a hub", modifier = Modifier.testTag("choose-hub"), style = MaterialTheme.typography.headlineSmall)
            Info("Hubs", HUB_MEANS)
        }
        Group("") { FoundHubs(engine, ui, auto = true) }
        Group("Or its address") { AddHub(engine, "Connect") }
    }
}
