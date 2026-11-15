package org.licash.wallet.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material.icons.filled.Warning
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import org.licash.core.Network

/** The name, as a mark. */
@Composable
fun Wordmark(modifier: Modifier = Modifier, size: Int = 22) {
    Text(
        "licash",
        modifier = modifier,
        fontSize = size.sp,
        fontWeight = FontWeight.Bold,
        letterSpacing = (-0.5).sp,
        color = MaterialTheme.colorScheme.primary,
    )
}

/** The network, unless it is Bitcoin's own. */
@Composable
fun NetworkChip(network: Network) {
    if (network == Network.BITCOIN) return
    Surface(
        shape = RoundedCornerShape(50),
        color = MaterialTheme.colorScheme.secondary.copy(alpha = 0.18f),
    ) {
        Text(
            network.name,
            modifier = Modifier.padding(horizontal = 10.dp, vertical = 3.dp),
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.secondary,
        )
    }
}

@Composable
fun TopBar(title: String, back: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().padding(vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        IconButton(onClick = back) {
            Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
        }
        Text(title, style = MaterialTheme.typography.titleLarge)
    }
}

/** A titled card of rows. */
@Composable
fun Group(title: String, content: @Composable ColumnScope.() -> Unit) {
    Spacer(Modifier.height(20.dp))
    // Untitled on a page of its own, the page's title saying it.
    if (title.isNotEmpty()) {
        Text(
            title,
            modifier = Modifier.padding(start = 4.dp, bottom = 8.dp),
            style = MaterialTheme.typography.labelLarge,
            color = MaterialTheme.colorScheme.primary,
        )
    }
    Surface(
        modifier = Modifier.fillMaxWidth(),
        shape = RoundedCornerShape(20.dp),
        color = MaterialTheme.colorScheme.surfaceContainer,
    ) {
        Column(Modifier.padding(horizontal = 16.dp, vertical = 4.dp), content = content)
    }
}

@Composable
fun Divider() = HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)

/** The one thing to do on a screen. */
@Composable
fun BigButton(text: String, modifier: Modifier = Modifier, enabled: Boolean = true, onClick: () -> Unit) {
    Button(
        onClick = onClick,
        enabled = enabled,
        modifier = modifier.height(56.dp),
        shape = RoundedCornerShape(18.dp),
    ) {
        Text(text, style = MaterialTheme.typography.labelLarge)
    }
}

/** Where a step of something under way stands. */
enum class Mark { DONE, NOW, NEXT, FAILED }

/**
 * A step of a timeline: a mark, joined to the next step's by a line unless
 * `last`, its title, what it says of it, and something to do, if any.
 */
@Composable
fun Step(
    mark: Mark,
    title: String,
    detail: String? = null,
    last: Boolean = false,
    action: Pair<String, () -> Unit>? = null,
) {
    val scheme = MaterialTheme.colorScheme
    Row(Modifier.fillMaxWidth().height(IntrinsicSize.Min).testTag("step $title")) {
        Column(
            Modifier.width(28.dp).fillMaxHeight(),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Spacer(Modifier.height(12.dp))
            Box(Modifier.size(20.dp), contentAlignment = Alignment.Center) {
                when (mark) {
                    Mark.DONE -> Icon(Icons.Filled.CheckCircle, "Done", tint = scheme.primary)
                    Mark.FAILED -> Icon(Icons.Filled.Warning, "Failed", tint = scheme.error)
                    Mark.NOW -> CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp)
                    Mark.NEXT -> Box(Modifier.size(14.dp).border(2.dp, scheme.outline, CircleShape))
                }
            }
            if (!last) {
                Spacer(Modifier.height(4.dp))
                val line = if (mark == Mark.DONE) scheme.primary else scheme.outlineVariant
                Box(Modifier.width(2.dp).weight(1f).background(line))
            }
        }
        Spacer(Modifier.width(12.dp))
        Column(Modifier.weight(1f).padding(top = 10.dp, bottom = if (last) 10.dp else 14.dp)) {
            Text(
                title,
                style = MaterialTheme.typography.bodyLarge,
                color = when (mark) {
                    Mark.NEXT -> scheme.onSurfaceVariant
                    Mark.FAILED -> scheme.error
                    else -> scheme.onSurface
                },
            )
            detail?.let {
                Text(it, style = MaterialTheme.typography.bodySmall, color = scheme.onSurfaceVariant)
            }
            action?.let { (label, act) ->
                TextButton(onClick = act, modifier = Modifier.height(36.dp)) { Text(label) }
            }
        }
    }
}
