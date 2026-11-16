package org.licash.wallet.ui

import android.net.Uri
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
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.dp
import org.licash.core.Contract
import org.licash.core.ContractQuote
import org.licash.core.ContractSafety
import org.licash.core.ContractState
import org.licash.core.Recovery
import org.licash.core.RecoveryStage
import org.licash.wallet.Proving
import org.licash.wallet.digits
import org.licash.wallet.sat
import org.licash.wallet.share
import org.licash.wallet.safetyStatus

/**
 * A protection at a hub, in a row: how much it covers, how long it has
 * left and where it stands, and what it is worth, never folded away. A
 * tap unfolds the rest. Once forced closed (`recovery`), where that
 * stands instead; in advanced mode, forcing it closed.
 */
@Composable
internal fun CoverRow(
    c: Contract,
    height: Int,
    developer: Boolean,
    recovery: Recovery?,
    end: () -> Unit,
    forceClose: () -> Unit,
    giveUp: () -> Unit,
    saveStatement: (Uri) -> Unit = {},
    bringProof: (Uri) -> Unit = {},
    proving: Proving? = null,
    proveHere: (() -> Unit)? = null,
    stopProving: () -> Unit = {},
    saveReceipt: (Uri) -> Unit = {},
    safety: suspend () -> ContractSafety? = { null },
) {
    val colors = MaterialTheme.colorScheme
    val cover = cover(c, height)
    val forcing = recovery?.let { forcing(it, height) }
    var open by rememberSaveable(c.id) { mutableStateOf(false) }
    var plan by remember(c.id) { mutableStateOf<ContractSafety?>(null) }
    LaunchedEffect(open, c.id) { if (open && recovery == null) plan = safety() }
    // While it is set up: the step it is at.
    val step = c.step?.let { setup(it) }
    val tint = when {
        forcing != null -> if (forcing.failed) colors.error else colors.tertiary
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
                (forcing?.says ?: step?.second ?: cover.left)?.let {
                    Text(
                        it,
                        modifier = if (step != null) Modifier.testTag("contract-step") else Modifier,
                        style = MaterialTheme.typography.bodySmall,
                        color = colors.onSurfaceVariant,
                    )
                }
            }
            Text(
                forcing?.stands ?: cover.stands,
                Modifier.testTag("contract-state"),
                style = MaterialTheme.typography.labelLarge,
                color = tint,
            )
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
        if (cover.runs && recovery == null) Worth(c.mock)
        AnimatedVisibility(open) {
            Column {
                if (recovery != null) {
                    Forced(recovery, developer, giveUp, height)
                    if (needsProof(recovery)) {
                        ProveElsewhere(recovery, saveStatement, bringProof)
                        proveHere?.let {
                            ProveHere(proving, "${recovery.id.take(16)}.receipt", it, stopProving, saveReceipt)
                        }
                    }
                } else {
                    Unfolded(c, cover, developer, end, forceClose, plan, height)
                }
            }
        }
    }
}

/** A protection's row unfolded: what it covers and costs; ending it, forcing it closed in advanced mode. */
@Composable
private fun Unfolded(c: Contract, cover: Cover, developer: Boolean, end: () -> Unit, forceClose: () -> Unit, safety: ContractSafety?, height: Int) {
    val state = c.state
    if (state is ContractState.Failed) Line("Why", state.reason)
    // Of what its hub holds, those ending first covering first.
    if (cover.on) Line("Covers now", sat(c.covers), tag = "covers")
    if (cover.ends != null) Line("Ends by itself in", cover.ends)
    if (!cover.failed) Line("Cost", sat(c.price), share = share(c.price, c.cap))
    if (cover.held) {
        safety?.let {
            Line("Recovery planning", safetyStatus(it, height))
            Line("Your proof margin", "${it.proofMargin} blocks")
            Say("Planning only. Start earlier for confirmation time. Stay online: a late hub answer can reduce your proving time until your timeout confirms.")
        }
    }
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
            // Its hub silent: what it holds claimed on Bitcoin, unilaterally.
            if (developer && cover.on) {
                TextButton(onClick = forceClose, modifier = Modifier.testTag("force-close")) {
                    Text("Force close", color = MaterialTheme.colorScheme.error)
                }
            }
        }
    }
}

/** A protection forced closed, unfolded: what it claimed, what stopped it, and giving it up while it may be. */
@Composable
private fun Forced(r: Recovery, developer: Boolean, giveUp: () -> Unit, height: Int) {
    Line("Claimed", sat(r.claimed), tag = "recovery-claimed")
    if (needsProof(r) && height > 0) r.deadline?.let {
        Line("Time left to prove", "${it.toLong().minus(height).coerceAtLeast(0)} blocks")
    }
    r.error?.let { Line("Stopped by", it) }
    if (developer) {
        r.nextAt?.let { Line("Next step at", "block $it") }
        r.request?.let { Line("Request", it.take(16) + "…") }
        r.publication?.let { Line("Claim", it.take(16) + "…") }
        r.timeout?.let { Line("Payback", it.take(16) + "…") }
        r.response?.let { Line("Hub's answer", it.take(16) + "…") }
        r.deadline?.let { Line("Prove by", "block $it") }
        r.assertion?.let { Line("Proof", it.take(16) + "…") }
        r.payout?.let { Line("Payout", it.take(16) + "…") }
        r.sweep?.let { Line("Sweep", it.take(16) + "…") }
    }
    if (forcing(r, height).revocable) {
        TextButton(onClick = giveUp, modifier = Modifier.testTag("give-up-recovery")) { Text("Cancel closing") }
    }
}

/** Cancel only this unsigned attempt, not the contract or the right to try again. */
@Composable
internal fun GiveUpSheet(recovery: Recovery?, busy: Boolean, confirm: () -> Unit, dismiss: () -> Unit) {
    val allowed = recovery?.let { forcing(it, 0).revocable } == true
    Sheet(if (allowed) "Cancel closing?" else "Closing changed", dismiss) {
        if (allowed && recovery != null) {
            Line("Back to wallet", sat(recovery.claimed))
            Line("Notes", digits(recovery.notes.toULong()))
            Say("Stops this attempt and releases its reserved notes. It does not close the contract or recover Bitcoin.")
            Say("You can force close again while the contract allows it. An offline hub still cannot pay you.")
            BigButton(
                if (busy) "Canceling…" else "Stop this attempt",
                Modifier.fillMaxWidth().testTag("give-up-recovery-confirm"),
                enabled = !busy,
                danger = true,
                onClick = confirm,
            )
            TextButton(onClick = dismiss, enabled = !busy, modifier = Modifier.fillMaxWidth()) { Text("Keep closing") }
        } else {
            Say("This attempt has moved on. It can no longer be canceled here.")
            BigButton("Done", Modifier.fillMaxWidth(), onClick = dismiss)
        }
    }
}

/**
 * What forcing a protection closed does, before it is done: for a hub that
 * stopped answering only, and final once its request is signed.
 */
@Composable
internal fun ForceSheet(c: Contract, held: ULong, force: () -> Unit, dismiss: () -> Unit) {
    val claimed = if (held < c.cap) held else c.cap
    Sheet("Force close", dismiss) {
        Say(
            "Only if ${name(c.hub)} stopped answering: while it answers, cash out instead. " +
                "Your ${sat(claimed)} there leave your balance and are claimed on Bitcoin, " +
                "back on Bitcoin in a few days if the hub stays silent. Its fees are paid from your Bitcoin.",
        )
        Say("Once its request is signed, it cannot be canceled, even before it is on Bitcoin.")
        BigButton("Force close", Modifier.fillMaxWidth().testTag("force-close-confirm"), danger = true) { force() }
    }
}

/** What a protection is worth for now, said where one shows and before one is paid for. */
@Composable
internal fun Worth(mock: Boolean) {
    Text(
        if (mock) MOCK else EXTERNAL_RECOVERY,
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
        Line("Cost", sat(q.price), tag = "quote-price", share = share(q.price, q.cap))
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
