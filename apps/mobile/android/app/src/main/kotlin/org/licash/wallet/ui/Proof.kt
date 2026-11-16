package org.licash.wallet.ui

import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.dp
import org.licash.core.Recovery
import org.licash.core.RecoveryStage
import org.licash.wallet.Proving
import org.licash.wallet.provingWords

/** Whether `recovery` waits for its proof, the statement to prove built. */
internal fun needsProof(recovery: Recovery): Boolean =
    recovery.stage == RecoveryStage.PROVING && recovery.deadline != null && !recovery.proved

/**
 * Proving a forced close's claim on a computer: its statement saved to a
 * file (`save`), proved there with `licash-r0`, the proof brought back from
 * the file it wrote (`bring`).
 */
@Composable
internal fun ProveElsewhere(recovery: Recovery, save: (Uri) -> Unit, bring: (Uri) -> Unit) {
    val saving = rememberLauncherForActivityResult(ActivityResultContracts.CreateDocument("application/octet-stream")) {
        it?.let(save)
    }
    val bringing = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { it?.let(bring) }
    Text(
        "Prove it on a computer with 16 GB of memory: save the statement (it shows your notes, keep it private), " +
            "run licash-r0 native-prove-succinct and native-wrap on it, then bring the proof back.",
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )
    Row {
        TextButton(
            onClick = { saving.launch("${recovery.id.take(16)}.statement.input") },
            modifier = Modifier.testTag("save-statement"),
        ) { Text("Save statement") }
        TextButton(
            onClick = { bringing.launch(arrayOf("*/*")) },
            modifier = Modifier.testTag("bring-proof"),
        ) { Text("Bring proof") }
    }
}

/**
 * Proving a forced close's claim on this phone, if this build carries the
 * prover: started, how far it got, stopped or carried on, and its receipt
 * saved once made (`receiptName`), for its last step on a computer.
 */
@Composable
internal fun ProveHere(
    proving: Proving?,
    receiptName: String,
    prove: () -> Unit,
    stop: () -> Unit,
    saveReceipt: (Uri) -> Unit,
) {
    val saving = rememberLauncherForActivityResult(ActivityResultContracts.CreateDocument("application/octet-stream")) {
        it?.let(saveReceipt)
    }
    when {
        proving == null -> {
            Note("Or prove it on this phone: many hours, best charging. Its last step still needs a computer.")
            TextButton(onClick = prove, modifier = Modifier.testTag("prove-here")) { Text("Prove on this phone") }
        }
        proving.done -> {
            Note(
                "Proved on this phone. Save its receipt, run licash-r0 native-wrap on it on an x86 computer " +
                    "with Docker, then bring the proof back.",
            )
            TextButton(onClick = { saving.launch(receiptName) }, modifier = Modifier.testTag("save-receipt")) {
                Text("Save receipt")
            }
        }
        else -> Progress(proving, prove, stop)
    }
}

/** A proof under way on this phone: how far, and stopped or carried on. */
@Composable
private fun Progress(proving: Proving, carryOn: () -> Unit, stop: () -> Unit) {
    Note(provingWords(proving), Modifier.testTag("proving"))
    LinearProgressIndicator(progress = { proving.share }, modifier = Modifier.fillMaxWidth().padding(vertical = 4.dp))
    if (proving.running) {
        TextButton(onClick = stop, modifier = Modifier.testTag("stop-proving")) { Text("Stop") }
    } else {
        TextButton(onClick = carryOn, modifier = Modifier.testTag("carry-on-proving")) { Text("Carry on") }
    }
}

/**
 * A sample statement this build carries proved on this phone, for a
 * developer to see how long a proof takes here and what it costs: not the
 * wallet's, nothing of it used.
 */
@Composable
internal fun SampleProof(proving: Proving?, prove: () -> Unit, stop: () -> Unit, again: () -> Unit) {
    Note("A sample claim, not yours: to see how long proving one takes on this phone.")
    when {
        proving == null -> TextButton(onClick = prove, modifier = Modifier.testTag("prove-sample")) {
            Text("Prove the sample")
        }
        proving.done -> {
            val pace = proving.segmentSecs?.let { " Parts took %.0f s each.".format(it) } ?: ""
            Note("${provingWords(proving)}.$pace")
            TextButton(onClick = again, modifier = Modifier.testTag("prove-sample-again")) { Text("Prove it again") }
        }
        else -> Progress(proving, prove, stop)
    }
}

@Composable
private fun Note(text: String, modifier: Modifier = Modifier) = Text(
    text,
    modifier = modifier,
    style = MaterialTheme.typography.bodySmall,
    color = MaterialTheme.colorScheme.onSurfaceVariant,
)
