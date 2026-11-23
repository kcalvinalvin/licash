import XCTest
@testable import Licash
import LicashCore

final class FormatTest: XCTestCase {
    func test_a_reason_is_a_sentence_with_its_amounts_grouped() {
        XCTAssertEqual(
            plain(reason: "hub takes at most 5000000 sat, not 12000000 sat"),
            "Hub takes at most 5,000,000 sat, not 12,000,000 sat"
        )
        // Short amounts and other numbers stay as they are.
        XCTAssertEqual(plain(reason: "lock expires at block 1200, 500 sat"), "Lock expires at block 1200, 500 sat")
    }

    func test_a_hub_charging_more_than_agreed_is_named_and_where_to_approve_it() {
        let said = explain(e: LicashError.FeesRaised(hub: "https://hub.example:8443"))
        XCTAssertEqual(
            said,
            "hub.example:8443 raised its fees since your protection there started. " +
                "Approve them in Settings, under Hubs, to pay there."
        )
    }

    func test_a_fee_says_what_it_is_of_the_amount() {
        XCTAssertEqual(share(fee: 0, of: 100), nil)
        XCTAssertEqual(share(fee: 5, of: 0), nil)
        XCTAssertEqual(share(fee: 1, of: 1_000_000), "<0.01%")
        XCTAssertEqual(share(fee: 24, of: 10_000), "0.24%")
        XCTAssertEqual(share(fee: 1, of: 100), "1%")
        XCTAssertEqual(share(fee: 12, of: 1_000), "1.2%")
        XCTAssertEqual(share(fee: 1, of: 20), "5%")
        XCTAssertEqual(share(fee: 999, of: 10_000), "10%")
        XCTAssertEqual(share(fee: 1, of: 8), "13%")
        XCTAssertEqual(share(fee: 30, of: 1), "3,000%")
        XCTAssertEqual(feeOf(fee: 12, of: 5_000), "12 sat · 0.24%")
        XCTAssertEqual(feeOf(fee: 0, of: 5_000), "0 sat")
    }

    func test_a_balance_too_small_for_a_code_is_said_in_plain_words() {
        XCTAssertEqual(
            plain(reason: "no hub it names is one trusted holding 5000 and the fee"),
            "Your balance can't pay 5,000 sat and its fee here."
        )
        XCTAssertEqual(
            plain(reason: "no hub it names is one trusted holding 20 and the fee"),
            "Your balance can't pay 20 sat and its fee here."
        )
        // A code that once said where to pay on Bitcoin instead says no more of it.
        XCTAssertEqual(
            plain(reason: "no hub it names is one trusted holding 1500 and the fee: pay tb1qexample on Bitcoin"),
            "Your balance can't pay 1,500 sat and its fee here."
        )
    }

    func test_no_provider_is_said_without_naming_providers() {
        XCTAssertEqual(
            plain(reason: "no swap provider found pays 20000 sat out on Bitcoin now"),
            "No one can send 20,000 sat on Bitcoin right now. Try less, or later."
        )
        XCTAssertEqual(
            plain(reason: "no swap provider found moves 1500 sat from http://a:1 to http://b:2 now"),
            "No one can move 1,500 sat between your hubs right now. Try less, or later."
        )
    }

    func test_the_most_providers_pay_out_is_offered_only_below_what_was_asked() {
        XCTAssertTrue(noPayout(e: LicashError.Invalid(reason: "no swap provider found pays 20000 sat out on Bitcoin now")))
        XCTAssertFalse(noPayout(e: LicashError.Invalid(reason: "not an address")))
        XCTAssertEqual(offeredInstead(asked: 20_000, most: 15_000), 15_000)
        // Nothing asked (all of it), none found, or no less: nothing to offer.
        XCTAssertNil(offeredInstead(asked: nil, most: 15_000))
        XCTAssertNil(offeredInstead(asked: 20_000, most: nil))
        XCTAssertNil(offeredInstead(asked: 20_000, most: 20_000))
        XCTAssertNil(offeredInstead(asked: 20_000, most: 0))
    }
}
