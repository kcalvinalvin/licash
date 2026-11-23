import LicashCore
import XCTest
@testable import Licash

class FeesTest: XCTestCase {
    func test_relative_fees_round_up_and_add_the_base_like_the_sdk() {
        XCTAssertEqual(relativeFee(amount: 0, ppm: 5_000), 0)
        XCTAssertEqual(relativeFee(amount: 1, ppm: 1), 1)
        XCTAssertEqual(relativeFee(amount: 10_000, ppm: 5_000), 50)
        XCTAssertEqual(feeDue(amount: 10_000, rate: Rate(base: 10, ppm: 5_000)), 60)
        XCTAssertEqual(feeDue(amount: 1, rate: Rate(base: 10, ppm: 1)), 11)
        XCTAssertEqual(feeDue(amount: UInt64.max, rate: Rate(base: UInt64.max, ppm: UInt64.max)), UInt64.max)
    }

    func test_historical_fees_never_invent_a_rate_or_a_recipient_split() {
        let fee = recordedFee(amount: 1_000, fee: 12)
        XCTAssertNil(fee.rate)
        XCTAssertTrue(fee.components.isEmpty)
        XCTAssertTrue(fee.explanation.contains("not saved"))
        XCTAssertEqual(recordedFee(amount: 1_000, fee: 0).fee, 0)
        XCTAssertTrue(recordedFee(amount: 1_000, fee: 0).explanation.contains("may not be final"))
    }

    func test_a_payout_splits_its_fee_by_who_gets_it_leaving_out_parts_of_zero() {
        let quoted = payoutFee(amount: 10_000, fee: 160, hub: 10, provider: 100, network: 50, quoted: true)
        XCTAssertTrue(
            quoted.components.elementsEqual([("Hub", 10), ("Payout provider", 100), ("Bitcoin network", 50)], by: ==),
            "\(quoted.components)"
        )
        XCTAssertTrue(quoted.explanation.contains("its own coin"))
        // Before its lock is made, the hub has charged nothing yet.
        let recorded = payoutFee(amount: 10_000, fee: 150, hub: 0, provider: 100, network: 50, quoted: false)
        XCTAssertTrue(
            recorded.components.elementsEqual([("Payout provider", 100), ("Bitcoin network", 50)], by: ==),
            "\(recorded.components)"
        )
        XCTAssertTrue(recorded.explanation.contains("not saved"))
        XCTAssertNil(recorded.rate)
    }

    func test_parts_that_do_not_add_up_to_the_fee_are_not_shown() {
        XCTAssertTrue(payoutFee(amount: 10_000, fee: 99, hub: 10, provider: 100, network: 50, quoted: false).components.isEmpty)
        XCTAssertTrue(payoutFee(amount: 10_000, fee: 0, hub: 0, provider: 0, network: 0, quoted: true).components.isEmpty)
        XCTAssertTrue(feeParts(fee: UInt64.max - 1, parts: [("a", UInt64.max), ("b", 1)]).isEmpty)
        let swapped = swapFee(amount: 300, fee: 5, hub: 1, provider: 4)
        XCTAssertTrue(swapped.components.elementsEqual([("Hub", 1), ("Swap provider", 4)], by: ==), "\(swapped.components)")
        XCTAssertEqual(swapFee(amount: 300, fee: 5, hub: 1, provider: 3), recordedFee(amount: 300, fee: 5))
    }

    func test_a_contract_costs_its_opening_fee_and_the_interest_on_each_part_and_not_its_reserve() {
        let range = ContractRange(
            minCap: 131_071, maxCap: 16_777_215, minBlocks: 432, maxBlocks: 52_560, perCoin: 2, reserve: 20_000,
            fee: 3_000, ppmYear: 10_000, ppmYearFunded: 5_000, yearBlocks: 52_560
        )
        let fee = ContractFee(opening: 3_000, interest: 41_943, interestFunded: 20_971)
        let cost = contractCost(range: range, cap: 16_777_215, fee: fee, funded: 8_388_607)
        XCTAssertEqual(cost.fee, 65_914)
        XCTAssertTrue(
            cost.components.elementsEqual(
                [
                    ("Hub's opening fee", 3_000),
                    ("Interest on the hub's part", 41_943),
                    ("Interest on your part", 20_971),
                ],
                by: ==
            ),
            "\(cost.components)"
        )
        XCTAssertTrue(cost.explanation.contains("1% a year on the 8,388,608 sat the hub funds"), cost.explanation)
        XCTAssertTrue(cost.explanation.contains("0.5% a year on the 8,388,607 sat you fund yourself"), cost.explanation)
        XCTAssertTrue(cost.explanation.contains("your reserve, which comes back to you"), cost.explanation)
        XCTAssertEqual(cost.totalLabel, "Total cost")
        // None of hers: no interest on her part, and her rate unsaid.
        let alone = contractCost(range: range, cap: 131_071, fee: ContractFee(opening: 3_000, interest: 655, interestFunded: 0), funded: 0)
        XCTAssertEqual(alone.fee, 3_655)
        XCTAssertEqual(alone.components.map { $0.0 }, ["Hub's opening fee", "Interest on the hub's part"])
        XCTAssertTrue(!alone.explanation.contains("you fund"))
    }

    func test_setup_cost_is_not_labeled_a_transaction_fee() {
        let cost = setupCost(amount: 4_096, cost: 23_000)
        XCTAssertEqual(cost.totalLabel, "Total cost")
        XCTAssertEqual(cost.title, "Setup cost")
        XCTAssertNil(cost.rate)
    }

    func test_an_invoice_through_a_provider_splits_its_fee_and_through_a_gateway_does_not() {
        let gateway = InvoiceQuote(hub: "https://hub.example", amount: 1_000, fee: 5, description: nil, provider: nil)
        XCTAssertTrue(invoiceFee(quote: gateway).components.isEmpty)
        let provider = ViaProvider(url: "https://swaps.example", name: nil, fee: 12, lockFee: 1, refundBlocks: 200)
        let via = invoiceFee(quote: { var q = gateway; q.fee = 13; q.provider = provider; return q }())
        XCTAssertTrue(via.components.elementsEqual([("Hub", 1), ("Swap provider", 12)], by: ==), "\(via.components)")
        XCTAssertTrue(via.explanation.contains("routing"))
    }
}
