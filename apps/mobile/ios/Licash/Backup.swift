import Foundation
import LicashCore

/// Words a recovery phrase is made of.
let PHRASE_WORDS = 24

/// The words of a phrase as typed or pasted: lowercase, in order, numbers
/// and punctuation of a written list dropped ("1. abandon, 2. ability").
func phraseWords(text: String) -> [String] {
    text.lowercased().split(separator: #/[^a-z]+/#).map(String.init)
}

/// A full backup's file name, its network told apart from Bitcoin's.
func backupName(network: Network, on: Date) -> String {
    let name = network == .bitcoin ? "licash" : "licash-\(network)"
    return "\(name)-backup-\(on.formatted(Date.ISO8601FormatStyle(timeZone: .current).year().month().day())).licash"
}
