package org.licash.wallet.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import org.licash.core.ColdStorage
import org.licash.wallet.Engine
import org.licash.wallet.Ui
import org.licash.wallet.explain
import org.licash.wallet.sat

/** The kinds of address an xpub's wallet may use: native segwit or taproot. */
internal val COLD_SCRIPTS = listOf("wpkh" to "Native segwit (bc1q)", "tr" to "Taproot (bc1p)")

/**
 * Whether `key` needs the kind of its addresses chosen: an xpub or tpub
 * names either; a zpub or vpub native segwit alone.
 */
internal fun needsScript(key: String): Boolean = key.trim().let { it.startsWith("xpub") || it.startsWith("tpub") }

/** Rules typed in, if each is: a threshold, an amount kept below it, a fee limit above 0% and at most 100%. */
internal data class ColdForm(val threshold: ULong, val keep: ULong, val maxFeePercent: Double)

internal fun coldForm(threshold: String, keep: String, percent: String): ColdForm? {
    val t = threshold.trim().replace(",", "").toULongOrNull() ?: return null
    val k = keep.trim().replace(",", "").ifEmpty { "0" }.toULongOrNull() ?: return null
    val p = percent.trim().toDoubleOrNull() ?: return null
    if (k > t || !(p > 0.0 && p <= 100.0)) return null
    return ColdForm(t, k, p)
}

private const val COLD_MEANS =
    "Once your notes exceed the threshold, all but what you keep is swapped out to your own wallet " +
        "(cold storage) through a swap provider, within your fee limit, each time to a new address of it. " +
        "Licash only sees its public key: it can count what is there, never spend it."

private const val BRING_BACK =
    "To bring funds back, send them from your cold wallet to a Bitcoin address shown in Receive. " +
        "Licash finds the coin and offers to protect it or move it into notes."

/**
 * Cold storage: what the external wallet holds and where the next
 * withdrawal goes, with its rules; or a form to set it up.
 */
@Composable
internal fun ColdPage(engine: Engine, ui: Ui) {
    val cold = ui.cold
    var editing by remember { mutableStateOf(false) }
    if (cold == null || editing) {
        ColdSetup(engine, cold) { editing = false }
        return
    }
    Group("") {
        Line("Balance", sat(cold.balance), tag = "cold-balance")
        Divider()
        Line("Coins", "${cold.coins}")
        Divider()
        Line("Checked", cold.scanned?.let { "from block ${cold.birthday} to $it" } ?: "Not yet, on the next sync")
        cold.pending?.let {
            Divider()
            Line("Withdrawal", "Under way")
        }
    }
    Group("Rules") {
        Line("Withdraw past", sat(cold.threshold))
        Divider()
        Line("Keep", sat(cold.keep))
        Divider()
        Line("Most in fees", "${cold.maxFeePercent}%")
        Divider()
        Line("Next address", cold.next, share = cold.next)
        Divider()
        Line("Key", cold.key, share = cold.key)
    }
    Group("Bringing funds back") { Say(BRING_BACK) }
    val scope = rememberCoroutineScope()
    var error by remember { mutableStateOf<String?>(null) }
    BigButton("Change rules", Modifier.fillMaxWidth().testTag("cold-edit"), quiet = true) { editing = true }
    Spacer(Modifier.height(12.dp))
    BigButton("Stop withdrawing", Modifier.fillMaxWidth().testTag("cold-clear"), danger = true) {
        scope.launch { engine.clearCold().onFailure { error = explain(it) } }
    }
    error?.let { Problem(it) }
}

@Composable
private fun ColdSetup(engine: Engine, before: ColdStorage?, done: () -> Unit) {
    val scope = rememberCoroutineScope()
    var key by remember { mutableStateOf(before?.key ?: "") }
    var script by remember { mutableStateOf(before?.script ?: "wpkh") }
    var threshold by remember { mutableStateOf(before?.threshold?.toString() ?: "") }
    var keep by remember { mutableStateOf(before?.keep?.toString() ?: "0") }
    var percent by remember { mutableStateOf(before?.maxFeePercent?.toString() ?: "1") }
    var birthday by remember { mutableStateOf(before?.birthday?.toString() ?: "") }
    var error by remember { mutableStateOf<String?>(null) }
    val form = coldForm(threshold, keep, percent)
    val born = birthday.trim().ifEmpty { null }?.let { it.toUIntOrNull() ?: return@let null }
    val bornOk = birthday.isBlank() || born != null
    Say(COLD_MEANS)
    Column(Modifier.padding(vertical = 12.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
        ColdField(key, { key = it; error = null }, "Extended public key", "zpub, or xpub with its kind of address")
        if (needsScript(key)) {
            Segmented(COLD_SCRIPTS.map { it.second }, COLD_SCRIPTS.indexOfFirst { it.first == script }.coerceAtLeast(0)) {
                script = COLD_SCRIPTS[it].first
            }
            Spacer(Modifier.height(8.dp))
        }
        ColdField(threshold, { threshold = it }, "Withdraw past (sat)", "When your notes exceed this", number = true)
        ColdField(keep, { keep = it }, "Keep (sat)", "Left in notes after a withdrawal, at most the threshold", number = true)
        ColdField(percent, { percent = it }, "Most in fees (%)", "Of each withdrawal; skipped while it costs more", number = true)
        ColdField(
            birthday,
            { birthday = it },
            "Wallet birthday (block, optional)",
            "Where to start counting its coins; empty for now",
            number = true,
            failed = !bornOk,
        )
    }
    error?.let { Problem(it) }
    BigButton(
        "Save",
        Modifier.fillMaxWidth().testTag("cold-save"),
        enabled = key.isNotBlank() && form != null && bornOk,
    ) {
        val f = form ?: return@BigButton
        val chosen = if (needsScript(key)) script else null
        scope.launch {
            engine.setCold(key.trim(), chosen, f.threshold, f.keep, f.maxFeePercent, born)
                .onSuccess { done() }
                .onFailure { error = explain(it) }
        }
    }
    if (before != null) {
        Spacer(Modifier.height(12.dp))
        BigButton("Cancel", Modifier.fillMaxWidth(), quiet = true) { done() }
    }
}

@Composable
private fun ColdField(
    value: String,
    change: (String) -> Unit,
    label: String,
    hint: String,
    number: Boolean = false,
    failed: Boolean = false,
) {
    OutlinedTextField(
        value = value,
        onValueChange = change,
        modifier = Modifier.fillMaxWidth(),
        singleLine = true,
        label = { Text(label) },
        supportingText = { Text(hint) },
        isError = failed,
        keyboardOptions = KeyboardOptions(
            keyboardType = if (number) KeyboardType.Decimal else KeyboardType.Ascii,
            imeAction = ImeAction.Done,
        ),
        shape = RoundedCornerShape(14.dp),
        textStyle = MaterialTheme.typography.bodyMedium,
    )
}
