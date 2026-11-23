import XCTest
@testable import Licash

class SendTest: XCTestCase {
    func test_a_payment_on_bitcoin_arrives_a_block_after_those_awaited() {
        XCTAssertEqual(arrives(depth: 0), "about 10 min")
        XCTAssertEqual(arrives(depth: 1), "about 20 min")
        XCTAssertEqual(arrives(depth: 6), "about 70 min")
    }

    func test_a_code_is_shown_short_whatever_its_scheme_case_or_amount() {
        let body = "tlicash1qqqqsqpqyqszqgpqyqszqgpqyqszqgpqyqszqgpqyqsabcdefgh"
        let short = "tlicash1qq…abcdefgh"
        XCTAssertEqual(shortCode(code: "licash:\(body)"), short)
        XCTAssertEqual(shortCode(code: " licash:\(body)?amount=5000 "), short)
        XCTAssertEqual(shortCode(code: "LICASH:\(body.uppercased())"), short)
        XCTAssertEqual(shortCode(code: body), short)
    }

    func test_the_same_blocks_take_a_twentieth_of_the_time_on_mutinynet() {
        XCTAssertEqual(arrives(depth: 0, blockSecs: 30), "about 1 min")
        XCTAssertEqual(arrives(depth: 1, blockSecs: 30), "about 1 min")
        XCTAssertEqual(arrives(depth: 6, blockSecs: 30), "about 4 min")
    }
}
