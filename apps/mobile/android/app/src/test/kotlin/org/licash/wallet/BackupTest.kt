package org.licash.wallet

import java.time.LocalDate
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.licash.core.Network
import org.licash.wallet.ui.autoBackupState

class BackupTest {
    @Test fun aWrittenListOfWordsReadsAsTheWordsAlone() {
        assertEquals(listOf("abandon", "ability", "zoo"), phraseWords(" 1. Abandon,\n2) ability\t 24. ZOO "))
        assertEquals(emptyList<String>(), phraseWords("  1. 2. "))
    }

    @Test fun aBackupFileTellsItsNetworkApartFromBitcoins() {
        val day = LocalDate.of(2026, 11, 16)
        assertEquals("licash-backup-2026-11-16.licash", backupName(Network.BITCOIN, day))
        assertEquals("licash-signet-backup-2026-11-16.licash", backupName(Network.SIGNET, day))
    }

    @Test fun automaticBackupsTellWhetherOnAndWhatFails() {
        assertEquals("Off: everything, to a folder you sync", autoBackupState(null))
        assertEquals("On, none written yet", autoBackupState(AutoBackup(at = null)))
        assertEquals("Failing: the folder is gone.", autoBackupState(AutoBackup(at = 1L, problem = "the folder is gone.")))
        assertTrue(autoBackupState(AutoBackup(at = 1_800_000_000_000L)).startsWith("On, last "))
    }
}
