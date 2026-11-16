package org.licash.wallet.ui

import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
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
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import java.time.LocalDate
import kotlinx.coroutines.launch
import org.licash.wallet.Engine
import org.licash.wallet.PHRASE_WORDS
import org.licash.wallet.Ui
import org.licash.wallet.backupName
import org.licash.wallet.explain
import org.licash.wallet.phraseWords

private const val ONE_COPY = "Use it on one phone only: a wallet live on two phones can lose money."

/** Settings' backup page: the recovery words, and a file of everything. */
@Composable
internal fun BackupPage(engine: Engine, ui: Ui) {
    var words by remember { mutableStateOf(false) }
    var file by remember { mutableStateOf(false) }
    Group("") {
        Entry("Recovery words", sub = "Your keys, on paper", tag = "backup-words") { words = true }
        Divider()
        Entry("Backup file", sub = "Everything, as of now", tag = "backup-file") { file = true }
    }
    if (words) Phrase(engine) { words = false }
    if (file) BackupFile(engine, ui) { file = false }
}

/** The 24 words, behind a warning and a tap; out of screenshots while shown. */
@Composable
private fun Phrase(engine: Engine, dismiss: () -> Unit) {
    val scope = rememberCoroutineScope()
    var words by remember { mutableStateOf<List<String>?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    Sheet("Recovery words", dismiss, secure = true) {
        val shown = words
        if (shown == null) {
            Say("Anyone with these words can take your money. Write them on paper and never share them.")
            Say("They bring back your keys and coins on Bitcoin. Money in hubs needs the backup file.")
            error?.let { Problem(it) }
            BigButton("Show words", Modifier.fillMaxWidth().testTag("show-words")) {
                error = null
                scope.launch { engine.backupPhrase().onSuccess { words = it }.onFailure { error = explain(it) } }
            }
        } else {
            WordList(shown)
            Spacer(Modifier.height(16.dp))
            BigButton("Done", Modifier.fillMaxWidth().testTag("words-done"), onClick = dismiss)
        }
    }
}

/** The words numbered, in two columns read down the first then the second. */
@Composable
private fun WordList(words: List<String>) {
    val half = (words.size + 1) / 2
    Row(Modifier.fillMaxWidth().testTag("words"), horizontalArrangement = Arrangement.spacedBy(24.dp)) {
        for (column in listOf(0 until half, half until words.size)) {
            Column(Modifier.weight(1f)) {
                for (i in column) {
                    Row(Modifier.padding(vertical = 4.dp)) {
                        Text(
                            "${i + 1}",
                            Modifier.width(28.dp),
                            style = MaterialTheme.typography.bodyLarge,
                            color = MaterialTheme.colorScheme.outline,
                        )
                        Text(words[i], style = MaterialTheme.typography.bodyLarge)
                    }
                }
            }
        }
    }
}

/** A file of everything the wallet holds, saved where the user picks. */
@Composable
private fun BackupFile(engine: Engine, ui: Ui, dismiss: () -> Unit) {
    val scope = rememberCoroutineScope()
    var busy by remember { mutableStateOf(false) }
    var saved by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    val saving = rememberLauncherForActivityResult(ActivityResultContracts.CreateDocument("application/octet-stream")) {
        uri ->
        if (uri != null) {
            busy = true
            error = null
            scope.launch {
                engine.exportBackup(uri).onSuccess { saved = true }.onFailure { error = explain(it) }
                busy = false
            }
        }
    }
    Sheet("Backup file", dismiss, dismissable = !busy) {
        if (saved) {
            Say("Saved. Save a new one after new payments.")
            BigButton("Done", Modifier.fillMaxWidth().testTag("backup-done"), onClick = dismiss)
            return@Sheet
        }
        Say("Everything this wallet holds, as of now. It is not encrypted: keep it private.")
        Say(ONE_COPY)
        error?.let { Problem(it) }
        BigButton(
            if (busy) "Saving…" else "Save file",
            Modifier.fillMaxWidth().testTag("save-backup"),
            enabled = !busy,
        ) { saving.launch(backupName(ui.network, LocalDate.now())) }
    }
}

/** No wallet yet: one brought back from a backup file, or from its 24 words. */
@Composable
internal fun Restore(engine: Engine, dismiss: () -> Unit) {
    val scope = rememberCoroutineScope()
    var typing by remember { mutableStateOf(false) }
    var text by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    fun run(restore: suspend () -> Result<Unit>) {
        busy = true
        error = null
        scope.launch {
            restore().onFailure { error = explain(it) }
            busy = false
        }
    }
    val opening = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        if (uri != null) run { engine.restoreBackup(uri) }
    }
    Sheet("Restore a wallet", dismiss, dismissable = !busy, secure = typing) {
        if (!typing) {
            Say(ONE_COPY)
            error?.let { Problem(it) }
            Entry("Backup file", sub = if (busy) "Restoring…" else "Everything, as saved", tag = "restore-file") {
                if (!busy) opening.launch(arrayOf("*/*"))
            }
            Divider()
            Entry("Recovery words", sub = "Your keys only", tag = "restore-words") { if (!busy) typing = true }
            return@Sheet
        }
        val words = phraseWords(text)
        Say("Brings back your keys and coins on Bitcoin, not money in hubs. Finding coins can take hours.")
        OutlinedTextField(
            value = text,
            onValueChange = { text = it; error = null },
            modifier = Modifier.fillMaxWidth().height(160.dp).testTag("phrase"),
            label = { Text("24 words") },
            supportingText = { Text("${words.size} of $PHRASE_WORDS") },
            isError = words.size > PHRASE_WORDS,
            keyboardOptions = KeyboardOptions(
                capitalization = KeyboardCapitalization.None,
                autoCorrectEnabled = false,
                keyboardType = KeyboardType.Password,
            ),
            shape = RoundedCornerShape(14.dp),
        )
        error?.let { Problem(it) }
        Spacer(Modifier.height(12.dp))
        BigButton(
            if (busy) "Restoring…" else "Restore",
            Modifier.fillMaxWidth().testTag("restore"),
            enabled = !busy && words.size == PHRASE_WORDS,
        ) { run { engine.restorePhrase(words) } }
    }
}

/** Advanced: look through Bitcoin again, after a yes. */
@Composable
internal fun ResetChain(engine: Engine) {
    val scope = rememberCoroutineScope()
    var asking by remember { mutableStateOf(false) }
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    Entry("Reset chain state", tag = "reset-chain") { error = null; asking = true }
    if (!asking) return
    Sheet("Reset chain state?", dismiss = { asking = false }, dismissable = !busy) {
        Say("Downloads Bitcoin's blocks again and looks for your payments from the start. Nothing you hold is lost; it can take a while.")
        error?.let { Problem(it) }
        BigButton(if (busy) "Resetting…" else "Reset", Modifier.fillMaxWidth().testTag("reset"), enabled = !busy) {
            busy = true
            scope.launch {
                engine.resetChain().onSuccess { asking = false }.onFailure { error = explain(it) }
                busy = false
            }
        }
        Spacer(Modifier.height(12.dp))
        BigButton("Cancel", Modifier.fillMaxWidth(), quiet = true, enabled = !busy) { asking = false }
    }
}
