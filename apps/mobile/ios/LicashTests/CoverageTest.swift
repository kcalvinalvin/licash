import XCTest
@testable import Licash
import LicashCore

final class CoverageTest: XCTestCase {
    private let one = "https://one.example"
    private let two = "https://two.example"

    private func contract(hub: String, cap: UInt64, covers: UInt64, state: ContractState = .protecting) -> Contract {
        Contract(
            id: "c\(cap)", hub: hub, state: state, mock: false, cap: cap, covers: covers, price: 1_000, funded: 0, fundedReceived: false,
            paidFrom: 50_000, protectsUntil: 2_000, closesAt: 2_100, funding: nil, createdAt: 0, step: nil,
            transfer: nil
        )
    }

    private func hub(terms: ContractTerms?) -> Hub {
        Hub(
            url: one, feeBase: 0, feePpm: 0, feeOutputMsat: 0, deposits: nil, silent: false, mailbox: false,
            lightningPay: nil, lockFee: Rate(base: 0, ppm: 0), refundFee: Rate(base: 0, ppm: 0), depositFee: nil, lightning: nil,
            contracts: terms
        )
    }

    func test_a_hub_counts_only_its_own_contracts_protecting_and_its_own_notes() {
        let contracts = [
            contract(hub: "\(one)/", cap: 4_096, covers: 3_000),
            contract(hub: one, cap: 8_192, covers: 0, state: .confirming),
            contract(hub: two, cap: 4_096, covers: 4_096),
        ]
        let notes = [
            Notes(hub: one, denomination: 1_024, count: 2),
            Notes(hub: one, denomination: 512, count: 3),
            Notes(hub: two, denomination: 64, count: 9),
        ]
        let cover = hubCover(hub: one, holding: Holding(hub: one, available: 4_000, pending: 1_000), contracts: contracts, notes: notes)
        XCTAssertEqual(cover, HubCover(held: 5_000, covered: 3_000, caps: 4_096, notes: 5))
        XCTAssertEqual(cover.missing, 2_000)
        XCTAssertEqual(cover.room, 1_096)
        XCTAssertEqual(cover.says, "3,000 of 5,000 sat protected, 2,000 sat not")
        XCTAssertEqual(cover.noteWords, "5 notes")
    }

    func test_it_says_what_is_missing_or_the_room_left() {
        XCTAssertNil(HubCover(held: 0, covered: 0, caps: 0, notes: 0).says)
        XCTAssertEqual(HubCover(held: 900, covered: 0, caps: 0, notes: 2).says, "Not protected")
        XCTAssertEqual(HubCover(held: 900, covered: 0, caps: 4_096, notes: 2).says, "900 sat not protected")
        XCTAssertEqual(HubCover(held: 900, covered: 900, caps: 4_096, notes: 2).says, "All protected, room for 3,196 sat more")
        XCTAssertEqual(HubCover(held: 4_096, covered: 4_096, caps: 4_096, notes: 2).says, "All protected, no room for more")
        XCTAssertEqual(HubCover(held: 900, covered: 0, caps: 0, notes: 1).noteWords, "1 note")
        // Never more covered than held, whatever the contracts report.
        XCTAssertEqual(
            hubCover(hub: one, holding: Holding(hub: one, available: 100, pending: 0), contracts: [contract(hub: one, cap: 4_096, covers: 500)], notes: []).covered,
            100
        )
    }

    func test_protecting_is_suggested_from_the_smallest_contract_until_dismissed() {
        let fixed = hub(terms: ContractTerms(cap: 4_096, price: 23_000, blocks: 1_005, mock: false, range: nil))
        let ranged = hub(
            terms: ContractTerms(
                cap: 4_096, price: 23_000, blocks: 1_005, mock: false,
                range: ContractRange(
                    minCap: 10_000, maxCap: 1_000_000, minBlocks: 432, maxBlocks: 52_560, perCoin: 2, reserve: 20_000, fee: 1_000,
                    ppmYear: 5_000, ppmYearFunded: 0, yearBlocks: 52_560
                )
            )
        )
        let cover = { (missing: UInt64) in HubCover(held: missing + 1_000, covered: 1_000, caps: 4_096, notes: 3) }
        XCTAssertEqual(smallestContract(hub: fixed), 4_096)
        XCTAssertEqual(smallestContract(hub: ranged), 10_000)
        XCTAssertNil(smallestContract(hub: hub(terms: nil)))
        XCTAssertFalse(suggestProtecting(cover: cover(4_095), hub: fixed, dismissed: nil))
        XCTAssertTrue(suggestProtecting(cover: cover(4_096), hub: fixed, dismissed: nil))
        XCTAssertFalse(suggestProtecting(cover: cover(9_999), hub: ranged, dismissed: nil))
        XCTAssertTrue(suggestProtecting(cover: cover(10_000), hub: ranged, dismissed: nil))
        // No contracts there: nothing to suggest.
        XCTAssertFalse(suggestProtecting(cover: cover(1_000_000), hub: hub(terms: nil), dismissed: nil))
        // Dismissed at 5,000: not again until more is not protected.
        XCTAssertFalse(suggestProtecting(cover: cover(5_000), hub: fixed, dismissed: 5_000))
        XCTAssertFalse(suggestProtecting(cover: cover(4_500), hub: fixed, dismissed: 5_000))
        XCTAssertTrue(suggestProtecting(cover: cover(5_001), hub: fixed, dismissed: 5_000))
    }

    func test_a_contract_opening_adds_its_cover_and_her_part() {
        // Nothing held: her part joins, all of it covered, the rest room.
        let none = HubCover(held: 0, covered: 0, caps: 0, notes: 0)
        XCTAssertEqual(coverAfter(now: none, cap: 4_194_303, funded: 2_094_006), HubCover(held: 2_094_006, covered: 2_094_006, caps: 4_194_303, notes: 0))
        XCTAssertEqual(coverAfter(now: none, cap: 4_194_303, funded: 2_094_006).says, "All protected, room for 2,100,297 sat more")
        // More held than it covers: the rest still not.
        let held = HubCover(held: 10_000_000, covered: 1_000_000, caps: 1_048_575, notes: 9)
        XCTAssertEqual(coverAfter(now: held, cap: 2_097_151, funded: 500_000), HubCover(held: 10_500_000, covered: 3_097_151, caps: 3_145_726, notes: 9))
    }
}
