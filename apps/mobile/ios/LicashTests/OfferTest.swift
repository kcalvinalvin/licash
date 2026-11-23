import LicashCore
import XCTest
@testable import Licash

class OfferTest: XCTestCase {
    private func offer(coins: String...) -> ContractQuote {
        ContractQuote(
            hub: "https://hub.example",
            price: 23_000,
            cap: 131_071,
            blocks: 1_000,
            protectsFor: 820,
            mock: false,
            coin: 10_000_000,
            download: 100_100_000,
            range: nil,
            afterAssert: 180,
            coins: coins.enumerated().map { i, c in DepositCoin(outpoint: c, value: 10_000_000 - UInt64(i)) },
            suggestedCap: 15_000_000,
            suggestedBlocks: 26_280,
            dust: 330,
            denominations: (0...31).map { UInt64(1) << $0 },
            fundedNotes: 32
        )
    }

    func test_an_offer_is_for_the_largest_coin() {
        XCTAssertEqual(offeredCoin(q: offer(coins: "a:0", "b:1")), "a:0")
        XCTAssertNil(offeredCoin(q: offer()))
    }

    func test_an_offer_declined_is_not_made_unasked_again_until_another_coin_comes() {
        XCTAssertTrue(offerDeclined(q: offer(coins: "a:0", "b:1"), declined: "a:0"))
        // A larger coin paid since: offered again.
        XCTAssertFalse(offerDeclined(q: offer(coins: "c:0", "a:0"), declined: "a:0"))
        // None declined, or none offered.
        XCTAssertFalse(offerDeclined(q: offer(coins: "a:0"), declined: nil))
        XCTAssertFalse(offerDeclined(q: nil, declined: "a:0"))
        XCTAssertFalse(offerDeclined(q: offer(), declined: "a:0"))
    }
}
