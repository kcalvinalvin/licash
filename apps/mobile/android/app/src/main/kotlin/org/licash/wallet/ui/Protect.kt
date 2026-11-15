package org.licash.wallet.ui

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.KeyboardArrowUp
import androidx.compose.material3.Icon
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.dp
import org.licash.core.Contract
import org.licash.core.ContractQuote
import org.licash.core.ContractState
import org.licash.wallet.sat

/**
 * A protection at a hub, in a row: how much it covers, how long it has
 * left and where it stands, and what it is worth, never folded away. A
 * tap unfolds the rest.
 */
@Composable
internal fun CoverRow(c: Contract, height: Int, developer: Boolean, end: () -> Unit) {
    val colors = MaterialTheme.colorScheme
    val cover = cover(c, height)
    var open by rememberSaveable(c.id) { mutableStateOf(false) }
    // While it is set up: the step it is at.
    val step = c.step?.let { setup(it) }
    val tint = when {
        cover.failed -> colors.error
        cover.on -> colors.primary
        else -> colors.onSurfaceVariant
    }
    Column(Modifier.fillMaxWidth().testTag("contract")) {
        Row(
            Modifier.fillMaxWidth().clickable { open = !open }.heightIn(min = 56.dp).padding(vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Icon(
                if (cover.on) Glyphs.shield else Glyphs.shieldOff,
                contentDescription = null,
                Modifier.size(20.dp),
                tint = tint,
            )
            Spacer(Modifier.width(12.dp))
            Column(Modifier.weight(1f)) {
                Text("Up to ${sat(c.cap)}", style = MaterialTheme.typography.bodyLarge, maxLines = 1)
                (step?.second ?: cover.left)?.let {
                    Text(
                        it,
                        modifier = if (step != null) Modifier.testTag("contract-step") else Modifier,
                        style = MaterialTheme.typography.bodySmall,
                        color = colors.onSurfaceVariant,
                    )
                }
            }
            Text(cover.stands, Modifier.testTag("contract-state"), style = MaterialTheme.typography.labelLarge, color = tint)
            Icon(
                if (open) Icons.Default.KeyboardArrowUp else Icons.Default.KeyboardArrowDown,
                contentDescription = if (open) "Hide" else "Show",
                tint = colors.onSurfaceVariant,
            )
        }
        // What it is worth, in sight while it runs: nobody is to count on what protects nothing.
        step?.let { (done, _) ->
            LinearProgressIndicator(
                progress = { done / SETUP_STEPS },
                modifier = Modifier.fillMaxWidth().padding(bottom = 8.dp),
            )
        }
        if (cover.runs) Worth(c.mock)
        AnimatedVisibility(open) {
            Column {
                val state = c.state
                if (state is ContractState.Failed) Line("Why", state.reason)
                // Of what its hub holds, those ending first covering first.
                if (cover.on) Line("Covers now", sat(c.covers), tag = "covers")
                if (cover.ends != null) Line("Ends by itself in", cover.ends)
                if (!cover.failed) Line("Cost", sat(c.price))
                if (developer) {
                    Line("Paid from", "A coin of ${sat(c.paidFrom)}")
                    Line("Protects until", "block ${c.protectsUntil}")
                    Line("Closes at", "block ${c.closesAt}")
                    c.funding?.let { Line("Funding", it.take(16) + "…") }
                    Line("Id", c.id.take(16) + "…")
                }
                if (cover.held) {
                    Row {
                        TextButton(onClick = end, modifier = Modifier.testTag("close-contract")) { Text("End") }
                        if (developer) TextButton(onClick = {}, enabled = false) { Text("Force close") }
                    }
                }
            }
        }
    }
}

/** What a protection is worth for now, said where one shows and before one is paid for. */
@Composable
internal fun Worth(mock: Boolean) {
    Text(
        if (mock) MOCK else UNRECOVERED,
        modifier = Modifier.padding(bottom = 8.dp).testTag("worth"),
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.error,
    )
}

/**
 * What protecting at a hub covers, lasts and costs, before it is paid:
 * to pay it, or to get a coin on Bitcoin that pays it first.
 */
@Composable
internal fun ProtectSheet(q: ContractQuote, open: () -> Unit, cashOutFirst: () -> Unit, dismiss: () -> Unit) {
    val download = android.text.format.Formatter.formatShortFileSize(LocalContext.current, q.download.toLong())
    Sheet("Protect at ${name(q.hub)}", dismiss) {
        Line("Covers up to", sat(q.cap), tag = "quote-cap")
        Line("For", span(q.protectsFor.toLong()), tag = "quote-for")
        Line("Cost", sat(q.price), tag = "quote-price")
        Line("Takes", "Up to an hour, $download")
        Worth(q.mock)
        Spacer(Modifier.height(8.dp))
        if (q.coin != null) {
            BigButton("Protect for ${sat(q.price)}", Modifier.fillMaxWidth().testTag("protect")) { open() }
        } else {
            Say("Paid from Bitcoin you hold, and you hold too little. Cash out to yourself first, then protect.")
            BigButton("Cash out ${sat(firstCoin(q.price))}", Modifier.fillMaxWidth().testTag("cash-out-first")) {
                cashOutFirst()
            }
        }
    }
}

/** Why a protection does not end at once, and what to do instead: cash out what its hub holds. */
@Composable
internal fun EndSheet(c: Contract, height: Int, held: ULong, cashOut: () -> Unit, dismiss: () -> Unit) {
    val ends = cover(c, height).ends
    Sheet("Ends by itself", dismiss) {
        Say(
            (if (ends != null) "In $ends, and no sooner. " else "It cannot end sooner. ") +
                "To stop relying on ${name(c.hub)}, cash out what you hold there.",
        )
        if (held > 0uL) {
            BigButton("Cash out ${sat(held)}", Modifier.fillMaxWidth().testTag("end-cash-out")) { cashOut() }
        }
    }
}
