import LicashCore
import XCTest
@testable import Licash

final class HubsTest: XCTestCase {
    func test_optimization_shows_only_its_fee_and_never_a_zero_amount_payment() {
        XCTAssertEqual(optimizationCost(fee: 1, state: .done), "Fee 1 sat")
        XCTAssertEqual(optimizationCost(fee: 1, state: .pending), "Fee 1 sat")
        XCTAssertEqual(optimizationCost(fee: 0, state: .done), "Free")
        XCTAssertEqual(optimizationCost(fee: 1, state: .failed), "Not charged")
    }

    func test_protection_interest_is_said_short_and_on_hers_only_if_charged() {
        let range = ContractRange(
            minCap: 131_071, maxCap: 16_777_215, minBlocks: 432, maxBlocks: 52_560, perCoin: 2,
            reserve: 20_000, fee: 3_000, ppmYear: 10_000, ppmYearFunded: 0, yearBlocks: 52_560
        )
        XCTAssertEqual(interestOf(range: range), "1% a year")
        var funded = range
        funded.ppmYearFunded = 5_000
        XCTAssertEqual(interestOf(range: funded), "1% a year, 0.5% on yours")
    }

    private func found(silent: UInt32, onion: UInt32) -> HubsFound { HubsFound(hubs: [], silent: silent, silentOnion: onion) }

    private func hub(
        base: UInt64 = 0,
        ppm: UInt64 = 0,
        lightning: Bool = false,
        contracts: Bool = false,
        note: UInt64 = 0
    ) -> Hub {
        Hub(
            url: "https://hub.example", feeBase: base, feePpm: ppm, feeOutputMsat: note, deposits: nil, silent: false,
            mailbox: true,
            lightningPay: nil, lockFee: Rate(base: 0, ppm: 0), refundFee: Rate(base: 0, ppm: 0), depositFee: nil,
            lightning: lightning
                ? LightningTerms(pay: Rate(base: 2, ppm: 5_000), receive: Rate(base: 0, ppm: 0), receiveRange: LicashCore.Range(min: 1, max: 1_000_000))
                : nil,
            contracts: contracts ? ContractTerms(cap: 4_096, price: 23_000, blocks: 1_005, mock: true, range: nil) : nil
        )
    }

    func test_a_hub_is_told_by_its_fee_and_what_else_it_does() {
        XCTAssertEqual(about(hub: hub()), "No fee")
        XCTAssertEqual(about(hub: hub(ppm: 5_000)), "Fee 0.5%")
        XCTAssertEqual(about(hub: hub(base: 1, ppm: 5_000, lightning: true)), "Fee 1 sat + 0.5% · Lightning")
        XCTAssertEqual(about(hub: hub(lightning: true, contracts: true)), "No fee · Lightning · Protection")
    }

    func test_a_fee_on_the_notes_a_payment_makes_is_said_beside_its_rate() {
        XCTAssertEqual(paying(hub: hub()), "None")
        XCTAssertEqual(paying(hub: hub(base: 1, ppm: 100)), "1 sat + 0.01%")
        XCTAssertEqual(paying(hub: hub(base: 1, ppm: 100, note: 100)), "1 sat + 0.01% + 0.1 sat a note")
        XCTAssertEqual(paying(hub: hub(note: 2_000)), "2 sat a note")
        XCTAssertEqual(paying(hub: hub(note: 1_250)), "1.25 sat a note")
        XCTAssertEqual(about(hub: hub(note: 100)), "Fee 0.1 sat a note")
    }

    func test_a_hub_charging_more_than_agreed_is_told_by_the_fees_it_changed() {
        let agreed = LicashCore.Fees(spend: Rate(base: 1, ppm: 100), lock: Rate(base: 1, ppm: 0), refund: Rate(base: 0, ppm: 0), outputMsat: 100)
        func raised(charged: LicashCore.Fees) -> [Raise] { raises(raised: FeesRaised(hub: "https://hub.example", charged: charged, agreed: agreed)) }
        XCTAssertEqual(
            raised(charged: { var c = agreed; c.spend = Rate(base: 1, ppm: 200); return c }()),
            [Raise(what: "Paying", now: "1 sat + 0.02% + 0.1 sat a note", agreed: "1 sat + 0.01% + 0.1 sat a note")]
        )
        // The fee on each note made is a payment's too.
        XCTAssertEqual(
            raised(charged: { var c = agreed; c.outputMsat = 1_000; return c }()),
            [Raise(what: "Paying", now: "1 sat + 0.01% + 1 sat a note", agreed: "1 sat + 0.01% + 0.1 sat a note")]
        )
        XCTAssertEqual(
            raised(charged: { var c = agreed; c.lock = Rate(base: 2, ppm: 0); c.refund = Rate(base: 0, ppm: 5_000); return c }()),
            [Raise(what: "Holding a payment", now: "2 sat", agreed: "1 sat"), Raise(what: "Refunding one", now: "0.5%", agreed: "None")]
        )
        XCTAssertEqual(raised(charged: agreed), [])
    }

    func test_the_most_a_payment_costs_counts_the_notes_it_may_make() {
        // 1 sat and 0.01% of 10,000, and 28 notes at a tenth of a sat each.
        XCTAssertEqual(payingUpTo(hub: hub(base: 1, ppm: 100), amount: 10_000), 2)
        XCTAssertEqual(payingUpTo(hub: hub(base: 1, ppm: 100, note: 100), amount: 10_000), 5)
        XCTAssertEqual(payingUpTo(hub: hub(note: 100), amount: 0), 0)
        XCTAssertEqual(payingUpTo(hub: hub(), amount: 10_000), 0)
    }

    func test_hubs_that_all_answered_leave_nothing_to_say() {
        XCTAssertNil(unanswered(found: found(silent: 0, onion: 0), tor: false))
    }

    func test_those_that_did_not_answer_are_counted_and_tor_offered_when_it_would_reach_them() {
        XCTAssertEqual(unanswered(found: found(silent: 1, onion: 0), tor: false), "1 more did not answer")
        XCTAssertEqual(unanswered(found: found(silent: 3, onion: 0), tor: false), "3 more did not answer")
        XCTAssertEqual(unanswered(found: found(silent: 3, onion: 2), tor: false), "3 more did not answer: try with Tor on")
        XCTAssertEqual(unanswered(found: found(silent: 3, onion: 2), tor: true), "3 more did not answer")
    }

    func test_an_announced_name_is_shown_on_one_line_short_and_unturned() {
        XCTAssertEqual(shownName(announced: "  a\n test\thub "), "a test hub")
        XCTAssertEqual(shownName(announced: "evil\u{202E} hub"), "evil hub")
        XCTAssertEqual(shownName(announced: String(repeating: "x", count: 100))!.count, 40)
        XCTAssertNil(shownName(announced: " \u{200B} "))
    }
}
