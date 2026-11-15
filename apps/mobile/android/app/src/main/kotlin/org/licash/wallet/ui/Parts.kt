package org.licash.wallet.ui

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.selection.selectableGroup
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.KeyboardArrowUp
import androidx.compose.material.icons.filled.Warning
import androidx.compose.material.icons.outlined.Info
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.graphics.vector.addPathNodes
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.testTagsAsResourceId
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
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

/** A screen's title and the way back; `end` holds what else it offers, if anything. */
@Composable
fun TopBar(title: String, back: () -> Unit, end: @Composable RowScope.() -> Unit = {}) {
    Row(
        Modifier.fillMaxWidth().padding(vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        IconButton(onClick = back) {
            Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
        }
        Text(
            title,
            Modifier.weight(1f),
            style = MaterialTheme.typography.titleLarge,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        end()
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

/** The one thing to do on a screen; `quiet`, another beside it. */
@Composable
fun BigButton(
    text: String,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    quiet: Boolean = false,
    onClick: () -> Unit,
) {
    val shape = RoundedCornerShape(18.dp)
    val label: @Composable RowScope.() -> Unit = {
        Text(text, style = MaterialTheme.typography.labelLarge, maxLines = 1, overflow = TextOverflow.Ellipsis)
    }
    if (quiet) {
        FilledTonalButton(onClick, modifier.height(56.dp), enabled, shape, content = label)
    } else {
        Button(onClick, modifier.height(56.dp), enabled, shape, content = label)
    }
}

/**
 * A state in a glyph and a word or two, never by color alone: `busy` turns
 * the glyph into a spinner, and a tap, if it takes one, opens more. `tag`
 * names its words, for the tests to read.
 */
@Composable
fun Pill(
    icon: ImageVector,
    text: String,
    modifier: Modifier = Modifier,
    tint: Color = MaterialTheme.colorScheme.onSurfaceVariant,
    busy: Boolean = false,
    tag: String? = null,
    onClick: (() -> Unit)? = null,
) {
    val shape = RoundedCornerShape(50)
    val color = MaterialTheme.colorScheme.surfaceContainer
    val content: @Composable () -> Unit = {
        Row(
            Modifier.padding(start = 12.dp, end = if (onClick != null) 6.dp else 14.dp, top = 8.dp, bottom = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            if (busy) {
                CircularProgressIndicator(Modifier.size(16.dp), color = tint, strokeWidth = 2.dp)
            } else {
                Icon(icon, contentDescription = null, Modifier.size(18.dp), tint = tint)
            }
            Spacer(Modifier.width(8.dp))
            Text(
                text,
                Modifier.weight(1f, fill = false).then(if (tag != null) Modifier.testTag(tag) else Modifier),
                style = MaterialTheme.typography.labelLarge,
                color = tint,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            if (onClick != null) Chevron(tint.copy(alpha = 0.6f))
        }
    }
    if (onClick != null) {
        Surface(onClick = onClick, modifier = modifier, shape = shape, color = color, content = content)
    } else {
        Surface(modifier = modifier, shape = shape, color = color, content = content)
    }
}

/** What says a row opens something. */
@Composable
fun Chevron(tint: Color = MaterialTheme.colorScheme.outline) {
    Icon(Icons.AutoMirrored.Filled.KeyboardArrowRight, contentDescription = null, Modifier.size(20.dp), tint = tint)
}

/**
 * A row of a list: what it is and, at its end, its value; a glyph before
 * it and a line under it, if any. With something to open, a chevron.
 * `tag` names the value, for the tests to read, or the title of one
 * without a value.
 */
@Composable
fun Entry(
    title: String,
    modifier: Modifier = Modifier,
    value: String? = null,
    sub: String? = null,
    icon: ImageVector? = null,
    tint: Color = MaterialTheme.colorScheme.onSurfaceVariant,
    tag: String? = null,
    onClick: (() -> Unit)? = null,
) {
    Row(
        modifier.fillMaxWidth().then(if (onClick != null) Modifier.clickable(onClick = onClick) else Modifier)
            .heightIn(min = 56.dp).padding(vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        icon?.let {
            Icon(it, contentDescription = null, Modifier.size(20.dp), tint = tint)
            Spacer(Modifier.width(12.dp))
        }
        Column(Modifier.weight(1f)) {
            Text(
                title,
                modifier = if (tag != null && value == null) Modifier.testTag(tag) else Modifier,
                style = MaterialTheme.typography.bodyLarge,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            sub?.let {
                Text(
                    it,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
        value?.let {
            Spacer(Modifier.width(12.dp))
            Text(
                it,
                modifier = if (tag != null) Modifier.testTag(tag) else Modifier,
                style = Amounts,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                textAlign = TextAlign.End,
                maxLines = 1,
            )
        }
        if (onClick != null) {
            Spacer(Modifier.width(4.dp))
            Chevron()
        }
    }
}

/** A fact: its name, and what it is at the end. `tag` names the value, for the tests. */
@Composable
fun Line(label: String, value: String, modifier: Modifier = Modifier, tag: String? = null) {
    Row(modifier.fillMaxWidth().padding(vertical = 12.dp)) {
        Text(label, style = MaterialTheme.typography.bodyLarge)
        Spacer(Modifier.width(16.dp))
        Text(
            value,
            modifier = Modifier.weight(1f).then(if (tag != null) Modifier.testTag(tag) else Modifier),
            style = Amounts,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.End,
        )
    }
}

/**
 * What a tap asked for, from the bottom of the screen: a title and a
 * little under it. For what is read and left; what is worked in gets a
 * screen.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun Sheet(title: String, dismiss: () -> Unit, content: @Composable ColumnScope.() -> Unit) {
    ModalBottomSheet(
        onDismissRequest = dismiss,
        // All of it at once: no half-open sheet hiding its button.
        sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true),
        // Its own window: its test tags as resource ids too.
        modifier = Modifier.semantics { testTagsAsResourceId = true },
        containerColor = MaterialTheme.colorScheme.surfaceContainerLow,
    ) {
        Column(
            Modifier.fillMaxWidth().verticalScroll(rememberScrollState()).padding(horizontal = 24.dp)
                .padding(bottom = 32.dp),
        ) {
            Text(title, style = MaterialTheme.typography.titleLarge)
            Spacer(Modifier.height(12.dp))
            content()
        }
    }
}

/** A paragraph of a [Sheet]: what a word means, said once, where asked. */
@Composable
fun Say(text: String) {
    Text(
        text,
        modifier = Modifier.padding(bottom = 12.dp),
        style = MaterialTheme.typography.bodyLarge,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )
}

/** An (i): what `title` means, in a sheet, once tapped. */
@Composable
fun Info(title: String, text: String, modifier: Modifier = Modifier) {
    var shown by rememberSaveable { mutableStateOf(false) }
    IconButton(onClick = { shown = true }, modifier = modifier) {
        Icon(
            Icons.Outlined.Info,
            contentDescription = "About $title",
            Modifier.size(20.dp),
            tint = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
    if (shown) Sheet(title, dismiss = { shown = false }) { Say(text) }
}

/** What few ask for, folded under a word until tapped. */
@Composable
fun More(label: String = "More", modifier: Modifier = Modifier, content: @Composable ColumnScope.() -> Unit) {
    var open by rememberSaveable { mutableStateOf(false) }
    Row(
        modifier.fillMaxWidth().clip(RoundedCornerShape(12.dp)).clickable { open = !open }.heightIn(min = 48.dp)
            .padding(horizontal = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            label,
            Modifier.weight(1f),
            style = MaterialTheme.typography.labelLarge,
            color = MaterialTheme.colorScheme.primary,
        )
        Icon(
            if (open) Icons.Default.KeyboardArrowUp else Icons.Default.KeyboardArrowDown,
            contentDescription = if (open) "Hide" else "Show",
            tint = MaterialTheme.colorScheme.primary,
        )
    }
    AnimatedVisibility(open) { Column(content = content) }
}

/** One of a few, side by side: the one chosen stands out. Each is tagged `segment`, then its name. */
@Composable
fun Segmented(options: List<String>, chosen: Int, modifier: Modifier = Modifier, choose: (Int) -> Unit) {
    val colors = MaterialTheme.colorScheme
    Row(
        modifier.clip(RoundedCornerShape(50)).background(colors.surfaceContainer).padding(4.dp).selectableGroup(),
    ) {
        options.forEachIndexed { i, option ->
            val on = i == chosen
            Box(
                Modifier.weight(1f).height(44.dp).clip(RoundedCornerShape(50))
                    .background(if (on) colors.surfaceContainerHighest else Color.Transparent)
                    .selectable(selected = on, role = Role.Tab) { choose(i) },
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    option,
                    modifier = Modifier.testTag("segment $option"),
                    style = MaterialTheme.typography.labelLarge,
                    color = if (on) colors.onSurface else colors.onSurfaceVariant,
                    maxLines = 1,
                )
            }
        }
    }
}

/** Where a step of something under way stands. */
enum class Mark { DONE, NOW, NEXT, FAILED }

/**
 * A step of a timeline: a mark, joined to the next step's by a line unless
 * `last`, its title, what it says of it, and something to do, if any.
 * `tag` names it, for the tests.
 */
@Composable
fun Step(
    mark: Mark,
    title: String,
    detail: String? = null,
    last: Boolean = false,
    action: Pair<String, () -> Unit>? = null,
    tag: String = "step $title",
) {
    val scheme = MaterialTheme.colorScheme
    Row(Modifier.fillMaxWidth().height(IntrinsicSize.Min).testTag(tag)) {
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

/**
 * The few Material icons (Apache 2.0) the core set lacks, from their path
 * data: an extended set of thousands for a handful is not worth its size.
 */
internal object Glyphs {
    val up by lazy { icon("up", "M4,12l1.41,1.41L11,7.83V20h2V7.83l5.58,5.59L20,12l-8,-8 -8,8z") }
    val down by lazy { icon("down", "M20,12l-1.41,-1.41L13,16.17V4h-2v12.17l-5.58,-5.59L4,12l8,8 8,-8z") }
    val swap by lazy { icon("swap", "M6.99,11L3,15l3.99,4v-3H14v-2H6.99v-3zM21,9l-3.99,-4v3H10v2h7.01v3L21,9z") }
    val bolt by lazy {
        icon(
            "bolt",
            "M11,21h-1l1,-7H7.5c-0.58,0 -0.57,-0.32 -0.38,-0.66 0.19,-0.34 0.05,-0.08 0.07,-0.12" +
                "C8.48,10.94 10.42,7.54 13,3h1l-1,7h3.5c0.49,0 0.56,0.33 0.47,0.51l-0.07,0.15" +
                "C12.96,17.55 11,21 11,21z",
        )
    }

    /** Protected: a shield, checked. */
    val shield by lazy {
        icon(
            "shield",
            "M12,1L3,5v6c0,5.55 3.84,10.74 9,12 5.16,-1.26 9,-6.45 9,-12V5l-9,-4z" +
                "M10,17l-4,-4 1.41,-1.41L10,14.17l6.59,-6.59L18,9l-8,8z",
        )
    }

    /** Not protected: a shield's outline, empty. */
    val shieldOff by lazy {
        icon(
            "shieldOff",
            "M12,1L3,5v6c0,5.55 3.84,10.74 9,12 5.16,-1.26 9,-6.45 9,-12V5l-9,-4z" +
                "M19,11c0,4.52 -2.98,8.69 -7,9.93 -4.02,-1.24 -7,-5.41 -7,-9.93V6.3l7,-3.11 7,3.11V11z",
        )
    }

    private fun icon(name: String, path: String): ImageVector =
        ImageVector.Builder(name, 24.dp, 24.dp, 24f, 24f)
            .addPath(addPathNodes(path), fill = SolidColor(Color.Black))
            .build()
}
