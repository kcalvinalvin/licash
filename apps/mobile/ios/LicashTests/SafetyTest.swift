import XCTest
@testable import Licash
import LicashCore

final class SafetyTest: XCTestCase {
    func test_aPhoneOwnsItsMarginAndDaysAreOnlyAnEstimate() {
        // Counted in blocks, 144 a day on every network: Mutinynet's too.
        XCTAssertEqual(exitMarginBlocks(days: 14), 2_016)
        XCTAssertEqual(exitMarginBlocks(days: 7), 1_008)
    }

    func test_theSignedChallengeWindowIsShownAsEstimatedHours() {
        XCTAssertEqual(checkInHours(blocks: 144, blockSecs: 600), 24)
        XCTAssertEqual(checkInHours(blocks: 6, blockSecs: 600), 1)
        // The same blocks on Mutinynet, 30 seconds each: 72 minutes.
        XCTAssertEqual(checkInHours(blocks: 144, blockSecs: 30), 1)
        XCTAssertEqual(checkInHours(blocks: 2_880, blockSecs: 30), 24)
    }

    func test_equalityStillFitsAndOneBlockLaterIsInsideTheMargin() {
        let s = ContractSafety(requestBy: 7_966, assertHeight: 10_000, responseWindow: 12, exitMargin: 2_016, timeoutSlack: 6, challengeWindow: 144)
        XCTAssertEqual(safetyStatus(safety: s, height: 7_966), "Request should confirm by block 7966")
        XCTAssertEqual(safetyStatus(safety: s, height: 7_967), "Inside your recovery margin")
        XCTAssertEqual(safetyStatus(safety: s, height: 0), "Chain height unknown")
        XCTAssertEqual(safetyStatus(safety: { var c = s; c.requestBy = nil; return c }(), height: 100), "Margin does not fit this contract")
    }
}
