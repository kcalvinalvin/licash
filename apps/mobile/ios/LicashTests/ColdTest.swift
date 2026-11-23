import XCTest
@testable import Licash

final class ColdTest: XCTestCase {
    func test_an_xpub_needs_its_kind_of_address_chosen_and_a_zpub_does_not() {
        XCTAssertTrue(needsScript(key: " xpub6BgBgses"))
        XCTAssertTrue(needsScript(key: "tpubD6NzVbkr"))
        XCTAssertFalse(needsScript(key: "zpub6rFR7y4Q"))
        XCTAssertFalse(needsScript(key: "vpub5Y6cjg78"))
        XCTAssertFalse(needsScript(key: ""))
    }

    func test_rules_are_taken_only_when_each_makes_sense() {
        XCTAssertEqual(coldForm(threshold: "1,000,000", keep: "", percent: "1"), ColdForm(threshold: 1_000_000, keep: 0, maxFeePercent: 1.0))
        XCTAssertEqual(coldForm(threshold: "500", keep: "200", percent: "0.5"), ColdForm(threshold: 500, keep: 200, maxFeePercent: 0.5))
        // Keeping more than the threshold, or a fee limit of nothing or more than all.
        XCTAssertNil(coldForm(threshold: "500", keep: "600", percent: "1"))
        XCTAssertNil(coldForm(threshold: "500", keep: "0", percent: "0"))
        XCTAssertNil(coldForm(threshold: "500", keep: "0", percent: "101"))
        XCTAssertNil(coldForm(threshold: "", keep: "0", percent: "1"))
        XCTAssertNil(coldForm(threshold: "five", keep: "0", percent: "1"))
    }
}
