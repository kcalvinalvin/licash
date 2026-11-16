package org.licash.wallet

import java.time.LocalDate
import org.licash.core.Network

/** Words a recovery phrase is made of. */
const val PHRASE_WORDS = 24

/**
 * The words of a phrase as typed or pasted: lowercase, in order, numbers
 * and punctuation of a written list dropped ("1. abandon, 2. ability").
 */
fun phraseWords(text: String): List<String> =
    text.lowercase().split(Regex("[^a-z]+")).filter { it.isNotEmpty() }

/** A full backup's file name, its network told apart from Bitcoin's. */
fun backupName(network: Network, on: LocalDate): String {
    val name = if (network == Network.BITCOIN) "licash" else "licash-${network.name.lowercase()}"
    return "$name-backup-$on.licash"
}
