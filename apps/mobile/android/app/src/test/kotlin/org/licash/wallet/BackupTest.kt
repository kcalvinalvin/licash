package org.licash.wallet

import java.time.LocalDate
import org.junit.Assert.assertEquals
import org.junit.Test
import org.licash.core.Network

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
}
