import XCTest
@testable import Licash
import LicashCore

final class ChooseContractTest: XCTestCase {
    func test_durations_are_whole_days_within_the_range() {
        XCTAssertEqual(blocksAt(value: 400, least: 432, most: 52_560), 432)
        XCTAssertEqual(blocksAt(value: 26_250, least: 432, most: 52_560), 26_208)
        XCTAssertEqual(blocksAt(value: 26_300, least: 432, most: 52_560), 26_352)
        XCTAssertEqual(blocksAt(value: 52_560, least: 432, most: 52_560), 52_560)
        XCTAssertEqual(blocksAt(value: 1_000, least: 200, most: 1_000), 1_000)
        XCTAssertEqual(blocksAt(value: -5, least: 200, most: 1_000), 200)
    }

    // Caps of 2^n - 1 sat a coin of 1,000,000 sat may take whole, as a hub
    // allowing 4 times the coin fits them: the smallest at `least` and above.
    private let caps: [UInt64] = [1_048_575, 2_097_151]

    private func fake(least: UInt64) -> ContractFit? {
        caps.first { $0 >= least }.map { ContractFit(cap: $0, funded: 970_000) }
    }

    func test_every_cap_a_coin_takes_whole_is_offered_smallest_first() {
        XCTAssertEqual(fitsFrom(least: 131_071, fit: fake).map { $0.cap }, caps)
        XCTAssertEqual(fitsFrom(least: 131_071) { _ in nil }.map { $0.cap }, [UInt64]())
        // A fit below what was asked is not taken for another.
        XCTAssertEqual(fitsFrom(least: 5) { _ in ContractFit(cap: 4, funded: 1) }.map { $0.cap }, [UInt64]())
    }

    func test_the_cap_offered_is_half_again_the_coin_else_the_largest_allowed() {
        let fits = fitsFrom(least: 131_071, fit: fake)
        // 1.5 times 1,000,000 is past 1,048,575: the next.
        XCTAssertEqual(suggestedFit(fits: fits, coin: 1_000_000)?.cap, 2_097_151)
        // 600,000 half again is 900,000: the smallest covers it.
        XCTAssertEqual(suggestedFit(fits: fits, coin: 600_000)?.cap, 1_048_575)
        // None as large allowed: the largest.
        XCTAssertEqual(suggestedFit(fits: fits, coin: 1_500_000)?.cap, 2_097_151)
        XCTAssertNil(suggestedFit(fits: [], coin: 1_000_000))
    }

    func test_the_cover_is_chosen_first_and_decides_the_coins_paying_it() {
        let small = DepositCoin(outpoint: "a:0", value: 600_000)
        let large = DepositCoin(outpoint: "b:0", value: 1_000_000)
        let tiny = DepositCoin(outpoint: "c:0", value: 1_000)
        func fit(cap: UInt64) -> ContractFit { ContractFit(cap: cap, funded: 1) }
        let fits: [(DepositCoin, [ContractFit])] = [
            (large, [fit(cap: 1_048_575), fit(cap: 2_097_151)]),
            (small, [fit(cap: 1_048_575)]),
            (tiny, []),
        ]
        // Every cover some coin opens, each with the coins that pay it, smallest first.
        let covers = coversOf(fits: fits)
        XCTAssertEqual(covers.map { $0.0 }, [1_048_575, 2_097_151])
        XCTAssertEqual(covers.map { $0.1 }, [[small, large], [large]])
        // At first the one suggested for the smallest coin a contract takes whole.
        XCTAssertEqual(suggestedCover(fits: fits), 1_048_575)
        XCTAssertEqual(suggestedCover(fits: [(large, fits[0].1)]), 2_097_151)
        XCTAssertNil(suggestedCover(fits: [(tiny, [])]))
        XCTAssertTrue(coversOf(fits: []).isEmpty)
    }

    func test_what_she_holds_at_the_hub_counts_toward_the_cover_suggested() {
        let fits = fitsFrom(least: 131_071, fit: fake)
        // A small coin, 400,000 sat held there unprotected: half again both is past 1,048,575.
        XCTAssertEqual(suggestedFit(fits: fits, coin: 300_000, missing: 400_000)?.cap, 2_097_151)
        XCTAssertEqual(suggestedFit(fits: fits, coin: 300_000)?.cap, 1_048_575)
    }

    private let sizes: [UInt64] = [131_071, 262_143, 524_287, 1_048_575]

    // 3,000 sat and a sat of interest per 1,000 sat of the hub's part.
    private func price(cap: UInt64, funded: UInt64) -> UInt64 { 23_000 + (cap - funded) / 1_000 }

    func test_with_no_coin_she_cashes_out_one_paying_for_what_she_holds() {
        // 300,000 sat held unprotected: half again is 450,000, the hub funding nearly all of 524,287.
        let any = { (coin: UInt64, least: UInt64) in
            ContractFit(cap: least, funded: coin - self.price(cap: least, funded: FIXED_PART))
        }
        XCTAssertEqual(
            cashOutFirst(sizes: sizes, missing: 300_000, price: price, fit: any),
            CashOutFirst(cap: 524_287, coin: price(cap: 524_287, funded: FIXED_PART) + FIXED_PART)
        )
        // More than the largest: the largest.
        XCTAssertEqual(cashOutFirst(sizes: sizes, missing: 5_000_000, price: price, fit: any)?.cap, 1_048_575)
        // An older hub bounding the cap by 2 times the coin: the largest it takes, else none.
        let bounded = { (coin: UInt64, least: UInt64) in
            self.sizes.first { $0 >= least && $0 <= 2 * coin || $0 == 131_071 && $0 >= least }.map { ContractFit(cap: $0, funded: 1) }
        }
        XCTAssertEqual(cashOutFirst(sizes: sizes, missing: 300_000, price: price, fit: bounded)?.cap, 131_071)
        XCTAssertNil(cashOutFirst(sizes: sizes, missing: 300_000, price: price) { _, _ in nil })
        // Nothing missing there, nothing to protect.
        XCTAssertNil(cashOutFirst(sizes: sizes, missing: 0, price: price, fit: any))
        XCTAssertNil(cashOutFirst(sizes: [], missing: 300_000, price: price, fit: any))
    }

    func test_what_protection_asks_says_the_day_it_ends() {
        XCTAssertEqual(duty(ends: "May 21"), "Open Licash now and then, and before it ends on May 21.")
    }
}
