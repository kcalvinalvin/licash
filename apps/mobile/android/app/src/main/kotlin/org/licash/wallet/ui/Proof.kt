package org.licash.wallet.ui

import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Row
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import org.licash.core.Recovery
import org.licash.core.RecoveryStage

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
