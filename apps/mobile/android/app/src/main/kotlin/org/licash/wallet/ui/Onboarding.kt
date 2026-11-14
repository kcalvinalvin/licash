package org.licash.wallet.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import org.licash.core.Network
import org.licash.wallet.Engine
import org.licash.wallet.Ui
import org.licash.wallet.explain

/** No wallet yet: make one. Tapping the name 7 times shows the test networks. */
@Composable
fun Onboarding(engine: Engine, ui: Ui) {
    val scope = rememberCoroutineScope()
    var taps by remember { mutableIntStateOf(0) }
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    Column(
        Modifier.fillMaxSize().padding(horizontal = 32.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Spacer(Modifier.weight(1f))
        Wordmark(
            Modifier.clickable(remember { MutableInteractionSource() }, indication = null) {
                taps += 1
                if (taps >= 7 && !ui.developer) engine.setDeveloper(true)
            },
            size = 56,
        )
        Spacer(Modifier.height(12.dp))
        Text(
            "Bitcoin you can pay with in a tap.",
            style = MaterialTheme.typography.titleMedium,
            textAlign = TextAlign.Center,
        )
        Text(
            "One code to be paid at, on Bitcoin or in Licash alike.",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
        )
        if (ui.developer) {
            Spacer(Modifier.height(32.dp))
            Text("Network", style = MaterialTheme.typography.labelLarge)
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                for (network in Network.entries) {
                    FilterChip(
                        selected = ui.network == network,
                        onClick = { scope.launch { engine.setNetwork(network) } },
                        label = { Text(network.name.lowercase()) },
                    )
                }
            }
        }
        Spacer(Modifier.weight(1f))
        error?.let { Text(it, color = MaterialTheme.colorScheme.error, textAlign = TextAlign.Center) }
        BigButton(
            if (busy) "Creating…" else "Create wallet",
            Modifier.fillMaxWidth().padding(bottom = 32.dp),
            enabled = !busy,
        ) {
            busy = true
            error = null
            scope.launch {
                engine.create().onFailure { error = explain(it) }
                busy = false
            }
        }
    }
}
