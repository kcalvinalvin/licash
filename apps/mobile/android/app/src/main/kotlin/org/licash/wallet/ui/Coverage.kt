package org.licash.wallet.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.dp
import org.licash.core.Contract
import org.licash.core.ContractState
import org.licash.core.Holding
import org.licash.core.Hub
import org.licash.core.Notes
import org.licash.wallet.digits
import org.licash.wallet.sat

/*
 * How well each hub's notes are protected: what its contracts cover of what
 * is held there, what they leave out, and the room they have for more.
 * Pure, to be tested without a screen.
 */

/**
 * One hub's notes and their protection, in sat: `held` there, `covered` by
 * its contracts protecting now, and `caps`, the most those could cover;
 * `notes`, how many there are.
 */
data class HubCover(val held: ULong, val covered: ULong, val caps: ULong, val notes: ULong) {
    /** Held there and not covered: on the hub's word alone. */
    val missing: ULong get() = if (held > covered) held - covered else 0uL

    /** How much more its contracts could cover, were more held there. */
    val room: ULong get() = if (caps > covered) caps - covered else 0uL

    /**
     * In a line: how much is covered, then what is missing or the room
     * left; nothing to say while nothing is held or protects.
     */
    val says: String?
        get() = when {
            held == 0uL && caps == 0uL -> null
            caps == 0uL -> "Not protected"
            missing > 0uL && covered == 0uL -> "${sat(missing)} not protected"
            missing > 0uL -> "${digits(covered)} of ${sat(held)} protected, ${sat(missing)} not"
            room > 0uL -> "All protected, room for ${sat(room)} more"
            else -> "All protected, no room for more"
        }

    /** The notes held there, in words: `3 notes`. */
    val noteWords: String get() = if (notes == 1uL) "1 note" else "${digits(notes)} notes"
}

/**
 * What `contracts` protect at `hub` of what `holding` holds there, and how
 * many of `notes` are there.
 */
fun hubCover(hub: String, holding: Holding?, contracts: List<Contract>, notes: List<Notes>): HubCover {
    val held = holding?.let { it.available + it.pending } ?: 0uL
    val protecting = contracts.filter { sameHub(it.hub, hub) && it.state == ContractState.Protecting }
    val covered = protecting.sumOf { it.covers }.coerceAtMost(held)
    return HubCover(
        held = held,
        covered = covered,
        caps = protecting.sumOf { it.cap },
        notes = notes.filter { sameHub(it.hub, hub) }.sumOf { it.count },
    )
}

/**
 * The least a contract at `hub` protects, if it opens them: below that,
 * what is not protected is not worth a contract of its own.
 */
fun smallestContract(hub: Hub?): ULong? {
    val terms = hub?.contracts ?: return null
    return terms.range?.minCap ?: terms.cap
}

/**
 * Whether to suggest protecting or cashing out what `cover` leaves
 * unprotected at `hub`: once it reaches the hub's smallest contract, unless
 * the owner dismissed it at `dismissed` sat or more.
 */
fun suggestProtecting(cover: HubCover, hub: Hub?, dismissed: ULong?): Boolean {
    val smallest = smallestContract(hub) ?: return false
    return cover.missing >= smallest && (dismissed == null || cover.missing > dismissed)
}

/** What [suggestProtecting] says. */
fun unprotectedWords(missing: ULong, hub: String): String =
    "${sat(missing)} at ${name(hub)} is not protected. Protect it, or cash part of it out to Bitcoin."

/**
 * [HubCover] as a bar: covered, then missing, then the room left, each to
 * scale of the largest of what is held and what its contracts could cover.
 */
@Composable
fun CoverBar(cover: HubCover, modifier: Modifier = Modifier) {
    val whole = maxOf(cover.held, cover.covered + cover.room)
    if (whole == 0uL) return
    val colors = MaterialTheme.colorScheme
    Row(
        modifier.fillMaxWidth().height(6.dp).clip(RoundedCornerShape(3.dp)).background(colors.surfaceContainerHighest)
            .testTag("hub-cover-bar"),
    ) {
        for ((part, color) in listOf(
            cover.covered to colors.primary,
            cover.missing to colors.error,
            cover.room to colors.primaryContainer,
        )) {
            if (part > 0uL) {
                Box(
                    Modifier.weight(part.toFloat() / whole.toFloat()).fillMaxHeight().background(color),
                )
            }
        }
    }
}
