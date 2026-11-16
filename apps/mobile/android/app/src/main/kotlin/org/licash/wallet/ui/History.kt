package org.licash.wallet.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Warning
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import org.licash.core.Activity
import org.licash.core.Kind
import org.licash.core.State
import org.licash.wallet.Ui
import org.licash.wallet.key
import org.licash.wallet.sat

/**
 * Activity: what came in and went out, the newest first, what is on its
 * way before it. A line says what, when and how much; a tap, the rest.
 */
@Composable
fun History(ui: Ui, open: (Activity) -> Unit, back: () -> Unit) {
    val now = System.currentTimeMillis() / 1000
    LazyColumn(Modifier.fillMaxSize().padding(horizontal = 20.dp).testTag("history")) {
        item(key = "top") { TopBar("Activity", back) }
        if (ui.history.isEmpty() && ui.incoming.isEmpty()) {
            item(key = "none") {
                Text(
                    "Nothing yet",
                    modifier = Modifier.padding(horizontal = 4.dp, vertical = 24.dp),
                    style = MaterialTheme.typography.bodyLarge,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
        items(ui.incoming, key = { "coming " + it.id }) { activity ->
            ActivityRow(activity, now, coming = true) { open(activity) }
        }
        items(ui.history, key = { it.key }) { activity ->
            ActivityRow(activity, now) { open(activity) }
        }
    }
}

/**
 * A line of the history: an icon of its kind and direction, what it was,
 * its day, and its amount. One under way or failed says so in place of
 * its day.
 */
@Composable
private fun ActivityRow(activity: Activity, now: Long, coming: Boolean = false, open: () -> Unit) {
    val colors = MaterialTheme.colorScheme
    val failed = !coming && activity.state == State.FAILED
    val under = coming || activity.state == State.PENDING
    val sign = when {
        activity.kind == Kind.SWAP -> ""
        !coming && paid(activity) -> "−"
        else -> "+"
    }
    val tint = when {
        failed -> colors.error
        under -> colors.onSurfaceVariant
        sign == "+" -> colors.primary
        activity.kind == Kind.SWAP -> colors.secondary
        else -> colors.onSurfaceVariant
    }
    Row(
        Modifier.fillMaxWidth().clickable(onClick = open).heightIn(min = 64.dp).padding(horizontal = 4.dp, vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Surface(Modifier.size(40.dp), shape = CircleShape, color = tint.copy(alpha = 0.14f)) {
            Box(contentAlignment = Alignment.Center) {
                Icon(glyph(activity, coming, failed), contentDescription = null, Modifier.size(20.dp), tint = tint)
            }
        }
        Spacer(Modifier.width(14.dp))
        Column(Modifier.weight(1f)) {
            Text(
                title(activity, coming),
                style = MaterialTheme.typography.bodyLarge,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            when {
                failed -> Text("Failed", style = MaterialTheme.typography.bodySmall, color = colors.error)
                under -> Text(
                    "Pending",
                    modifier = Modifier.testTag("pending"),
                    style = MaterialTheme.typography.bodySmall,
                    color = colors.tertiary,
                )
                else -> activity.at?.let {
                    Text(day(it, now), style = MaterialTheme.typography.bodySmall, color = colors.onSurfaceVariant)
                }
            }
        }
        Spacer(Modifier.width(12.dp))
        Text(
            if (activity.kind == Kind.OPTIMIZED) optimizationCost(activity.fee, activity.state)
            else "$sign${sat(activity.amount)}",
            style = Amounts,
            color = when {
                failed -> colors.error
                under -> colors.onSurfaceVariant
                sign == "+" -> colors.primary
                else -> colors.onSurface
            },
        )
    }
}

/** The icon of a line of the history: failed, or its kind and direction. */
private fun glyph(activity: Activity, coming: Boolean, failed: Boolean): ImageVector = when {
    failed -> Icons.Default.Warning
    coming -> Glyphs.down
    else -> when (activity.kind) {
        Kind.LIGHTNING, Kind.INVOICE -> Glyphs.bolt
        Kind.SWAP, Kind.OPTIMIZED -> Glyphs.swap
        Kind.CONTRACT -> Glyphs.shield
        Kind.SENT, Kind.PAYOUT -> Glyphs.up
        Kind.RECEIVED, Kind.MOVED, Kind.ONCHAIN -> Glyphs.down
    }
}
