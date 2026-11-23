import LicashCore
import XCTest
@testable import Licash

class ExtraCodeTest: XCTestCase {
    private func extra(index: UInt32, label: String, payments: UInt32 = 0) -> ExtraCode {
        ExtraCode(index: index, label: label, createdAt: 0, code: nil, received: 0, payments: payments, fees: 0)
    }

    func test_a_label_is_its_words_one_space_apart_within_its_length_and_none_if_blank() {
        XCTAssertNil(labelOf(typed: " \t "))
        XCTAssertEqual(labelOf(typed: "  Bob \n online "), "Bob online")
        XCTAssertEqual(labelOf(typed: String(repeating: "x", count: MAX_CODE_LABEL + 5))!.count, MAX_CODE_LABEL)
    }

    func test_a_code_is_named_by_its_label_the_wallets_own_as_yours() {
        let codes = [extra(index: 1, label: "Forum"), extra(index: 2, label: "Shop")]
        XCTAssertEqual(codeName(index: 0, codes: codes), "Main identity")
        XCTAssertEqual(codeName(index: 2, codes: codes), "Shop")
        // Not listed yet: by its number.
        XCTAssertEqual(codeName(index: 3, codes: codes), "Identity 3")
    }

    func test_what_a_code_took_is_said_in_payments() {
        XCTAssertEqual(codeWords(code: extra(index: 1, label: "a")), "Anonymous, nothing received yet")
        XCTAssertEqual(codeWords(code: extra(index: 1, label: "a", payments: 1)), "Anonymous, 1 payment received")
        XCTAssertEqual(codeWords(code: extra(index: 1, label: "a", payments: 3)), "Anonymous, 3 payments received")
    }
}
