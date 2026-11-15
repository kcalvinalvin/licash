package org.licash.wallet.ui

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
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.saveable.rememberSaveable
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
import kotlinx.coroutines.launch
import org.licash.core.Activity
import org.licash.wallet.Engine
import org.licash.wallet.Ui
import org.licash.wallet.digits
import org.licash.wallet.explain
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
    money: () -> Unit,
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
            Spacer(Modifier.weight(1f))
            IconButton(onClick = settings) { Icon(Icons.Default.Settings, contentDescription = "Settings") }
        }
        // In the middle of what is left, scrolling on short screens and with large fonts.
        Column(
            Modifier.fillMaxWidth().weight(1f).verticalScroll(rememberScrollState()),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.Center,
        ) {
            Total(ui, money)
            Going(ui, money, history, open)
            if (ui.hubs.isEmpty()) {
                Spacer(Modifier.height(32.dp))
                ConnectHub(engine)
            }
        }
        if (ui.hubs.isNotEmpty()) {
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                BigButton("Receive", Modifier.weight(1f), onClick = receive)
                BigButton("Send", Modifier.weight(1f), onClick = send)
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
    val held = protection(ui.balance)
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
private fun Going(ui: Ui, money: () -> Unit, history: () -> Unit, open: (Activity) -> Unit) {
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
            is Glance.Protecting -> money
            is Glance.Behind -> ({ why = going.reason })
            Glance.Syncing -> null
        },
    )
    why?.let { Sheet("Not up to date", dismiss = { why = null }) { Say(it) } }
}

/** No hub yet: nothing works without one. */
@Composable
private fun ConnectHub(engine: Engine) {
    val scope = rememberCoroutineScope()
    var url by rememberSaveable { mutableStateOf("") }
    var busy by rememberSaveable { mutableStateOf(false) }
    var error by rememberSaveable { mutableStateOf<String?>(null) }
    Surface(shape = RoundedCornerShape(20.dp), color = MaterialTheme.colorScheme.surfaceContainer) {
        Column(Modifier.padding(20.dp)) {
            Text("Connect to a hub", style = MaterialTheme.typography.titleMedium)
            Text(
                "A hub holds your Licash and moves it for you. Paste the address its operator gave you.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.height(12.dp))
            OutlinedTextField(
                value = url,
                onValueChange = { url = it },
                modifier = Modifier.fillMaxWidth(),
                singleLine = true,
                placeholder = { Text("hub.example") },
            )
            error?.let { Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall) }
            Spacer(Modifier.height(12.dp))
            BigButton(if (busy) "Connecting…" else "Connect", Modifier.fillMaxWidth(), enabled = !busy && url.isNotBlank()) {
                busy = true
                error = null
                scope.launch {
                    engine.addHub(url.trim()).onFailure { error = explain(it) }
                    busy = false
                }
            }
        }
    }
}
