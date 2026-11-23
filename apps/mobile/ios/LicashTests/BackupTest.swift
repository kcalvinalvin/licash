import XCTest
@testable import Licash
import LicashCore

final class BackupTest: XCTestCase {
    func test_aWrittenListOfWordsReadsAsTheWordsAlone() {
        XCTAssertEqual(phraseWords(text: " 1. Abandon,\n2) ability\t 24. ZOO "), ["abandon", "ability", "zoo"])
        XCTAssertEqual(phraseWords(text: "  1. 2. "), [String]())
    }

    func test_aBackupFileTellsItsNetworkApartFromBitcoins() {
        let day = Calendar(identifier: .gregorian).date(from: DateComponents(year: 2026, month: 11, day: 16))!
        XCTAssertEqual(backupName(network: .bitcoin, on: day), "licash-backup-2026-11-16.licash")
        XCTAssertEqual(backupName(network: .signet, on: day), "licash-signet-backup-2026-11-16.licash")
    }

    func test_automaticBackupsTellWhetherOnAndWhatFails() {
        XCTAssertEqual(autoBackupState(backup: nil), "Off: everything, to a folder you sync")
        XCTAssertEqual(autoBackupState(backup: AutoBackup(at: nil)), "On, none written yet")
        XCTAssertEqual(autoBackupState(backup: AutoBackup(at: 1, problem: "the folder is gone.")), "Failing: the folder is gone.")
        XCTAssertTrue(autoBackupState(backup: AutoBackup(at: 1_800_000_000_000)).hasPrefix("On, last "))
    }
}
