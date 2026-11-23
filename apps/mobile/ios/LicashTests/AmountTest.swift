import XCTest
@testable import Licash

class AmountTest: XCTestCase {
    func test_only_digits_are_typed_no_leading_zeros_at_most_sixteen() {
        XCTAssertEqual(amountDigits(typed: "5,000 sat"), "5000")
        XCTAssertEqual(amountDigits(typed: "0012"), "12")
        XCTAssertEqual(amountDigits(typed: "000"), "")
        XCTAssertEqual(amountDigits(typed: "-.,"), "")
        XCTAssertEqual(amountDigits(typed: "12345678901234567"), "1234567890123456")
    }

    func test_thousands_are_set_apart_as_amounts_show_elsewhere() {
        for n: Int64 in [1, 12, 123, 1_234, 12_345, 123_456, 1_234_567, 2_100_000_000_000_000] {
            XCTAssertEqual(grouped(digits: String(n)).text, digits(amount: UInt64(n)))
        }
        XCTAssertEqual(grouped(digits: "").text, "")
    }

    func test_each_offset_lands_by_its_digit_past_the_commas() {
        // "1,234": before 1, before 2 (past the comma), before 3, before 4, past 4.
        XCTAssertEqual(grouped(digits: "1234").at, [0, 2, 3, 4, 5])
        XCTAssertEqual(grouped(digits: "123").at, [0, 1, 2, 3])
        XCTAssertEqual(grouped(digits: "12345678").at, [0, 1, 3, 4, 5, 7, 8, 9, 10])
        XCTAssertEqual(grouped(digits: "").at, [0])
    }
}
