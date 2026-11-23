import LicashCore
import XCTest
@testable import Licash

class CoverTest: XCTestCase {
    func test_a_corrected_recovery_shows_the_actual_sweep_not_the_original_claim() {
        let done = { var c = recovery(stage: .done, irrevocable: true, nextAt: nil); c.corrected = 0; c.received = 2_500; return c }()
        XCTAssertEqual(forcing(recovery: done, height: 1_000).says, "2,500 sat back on Bitcoin")
    }

    func test_note_count_counts_notes_not_their_denominations_across_hubs() {
        XCTAssertEqual(noteCount(notes: []), 0)
        XCTAssertEqual(noteCount(notes: [
            Notes(hub: "https://one.example", denomination: 64, count: 2),
            Notes(hub: "https://one.example", denomination: 4, count: 3),
            Notes(hub: "https://two.example", denomination: 32, count: 2),
        ]), 7)
    }

    private let created: Int64 = 1_800_000_000

    private func contract(state: ContractState, mock: Bool = true) -> Contract {
        Contract(
            id: "c0", hub: "https://hub.example", state: state, mock: mock, cap: 131_071, covers: 0, price: 23_000, funded: 0,
            fundedReceived: false, paidFrom: 33_000, protectsUntil: 1_861, closesAt: 2_005, funding: nil, createdAt: created,
            step: nil, transfer: nil
        )
    }

    func test_a_protection_says_where_it_stands_and_how_long_it_has_in_time() {
        let on = cover(contract: contract(state: .protecting), height: 1_000)
        XCTAssertEqual(on.stands, "Protecting")
        XCTAssertTrue(on.on && on.runs && on.held && !on.failed)
        XCTAssertEqual(on.left, "6 days left")
        XCTAssertEqual(on.ends, "7 days")

        let opening = cover(contract: contract(state: .opening), height: 1_000)
        XCTAssertEqual(opening.stands, "Setting up")
        XCTAssertTrue(opening.runs && !opening.on && !opening.held)

        let confirming = cover(contract: contract(state: .confirming), height: 1_000)
        XCTAssertEqual(confirming.stands, "Confirming")
        XCTAssertTrue(confirming.runs && confirming.held && !confirming.on)
    }

    func test_the_same_blocks_leave_a_twentieth_of_the_time_on_mutinynet() {
        // 861 blocks to go: 6 days at 10 minutes a block, 7 hours at 30 seconds (the close, 1 005, 8).
        let on = cover(contract: contract(state: .protecting), height: 1_000, blockSecs: 30)
        XCTAssertEqual(on.left, "7 hours left")
        XCTAssertEqual(on.ends, "8 hours")
    }

    func test_a_genuine_one_is_live_never_protecting() {
        let live = cover(contract: contract(state: .protecting, mock: false), height: 1_000)
        XCTAssertEqual(live.stands, "Live")
        XCTAssertTrue(live.on)
    }

    func test_one_past_protecting_has_nothing_left_but_still_ends() {
        let ending = cover(contract: contract(state: .ending), height: 1_900)
        XCTAssertEqual(ending.stands, "Ending")
        XCTAssertTrue(ending.held && !ending.runs && !ending.on)
        XCTAssertNil(ending.left)
        XCTAssertEqual(ending.ends, "18 hours")

        let ended = cover(contract: contract(state: .closed), height: 2_100)
        XCTAssertEqual(ended.stands, "Ended")
        XCTAssertFalse(ended.held)
        XCTAssertNil(ended.left)
        XCTAssertNil(ended.ends)

        let failed = cover(contract: contract(state: .failed(reason: "the hub refused")), height: 1_000)
        XCTAssertEqual(failed.stands, "Failed")
        XCTAssertTrue(failed.failed && !failed.held && !failed.runs)
        XCTAssertNil(failed.ends)
    }

    func test_no_time_is_said_while_the_height_is_unknown() {
        let on = cover(contract: contract(state: .protecting), height: 0)
        XCTAssertNil(on.left)
        XCTAssertNil(on.ends)
    }

    func test_ended_and_long_failed_ones_are_left_out_of_sight() {
        XCTAssertTrue(shown(contract: contract(state: .protecting), now: created + 1_000_000))
        XCTAssertTrue(shown(contract: contract(state: .ending), now: created + 1_000_000))
        XCTAssertFalse(shown(contract: contract(state: .closed), now: created))
        XCTAssertTrue(shown(contract: contract(state: .failed(reason: "no")), now: created + 86_399))
        XCTAssertFalse(shown(contract: contract(state: .failed(reason: "no")), now: created + 86_400))
    }

    func test_a_hub_is_named_by_its_host_an_onion_one_cut_short() {
        // XCTestCase's own `name` hides the app's in here.
        XCTAssertEqual(Licash.name(url: "https://hub.example"), "hub.example")
        XCTAssertEqual(Licash.name(url: "http://127.0.0.1:38338"), "127.0.0.1:38338")
        let onion = "abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx"
        XCTAssertEqual(Licash.name(url: "http://\(onion).onion"), "abcdefgh….onion")
        XCTAssertEqual(Licash.name(url: "http://\(onion).onion:8080"), "abcdefgh….onion:8080")
        XCTAssertEqual(Licash.name(url: "http://short.onion"), "short.onion")
    }

    func test_a_fee_is_said_as_short_as_it_can_be() {
        XCTAssertEqual(charge(rate: Rate(base: 0, ppm: 0)), "None")
        XCTAssertEqual(charge(rate: Rate(base: 10, ppm: 0)), "10 sat")
        XCTAssertEqual(charge(rate: Rate(base: 0, ppm: 5_000)), "0.5%")
        XCTAssertEqual(charge(rate: Rate(base: 10, ppm: 5_000)), "10 sat + 0.5%")
    }

    private func recovery(stage: RecoveryStage, irrevocable: Bool = false, nextAt: UInt32? = 1_144) -> Recovery {
        Recovery(
            id: "c0", stage: stage, irrevocable: irrevocable, claimed: 3_000, notes: 7, request: nil,
            publication: nil, timeout: nil, sweep: nil, nextAt: nextAt, error: nil
        )
    }

    func test_a_forced_close_says_where_it_stands_and_may_be_given_up_only_while_claimed() {
        let claimed = forcing(recovery: recovery(stage: .claimed), height: 1_000)
        XCTAssertEqual(claimed.stands, "Forcing closed")
        XCTAssertEqual(claimed.says, "Final in 24 hours: it can be given up until then")
        XCTAssertTrue(claimed.revocable && !claimed.failed)
        // Final already: claimed, not to be given up.
        XCTAssertFalse(forcing(recovery: recovery(stage: .claimed, irrevocable: true), height: 1_000).revocable)
        let requested = forcing(recovery: recovery(stage: .requested, irrevocable: true), height: 1_000)
        XCTAssertEqual(requested.says, "The hub has 24 hours to answer")
        XCTAssertFalse(requested.revocable)
        // The chain's height unknown: no wait said.
        XCTAssertNil(forcing(recovery: recovery(stage: .claimed), height: 0).says)
        XCTAssertEqual(forcing(recovery: recovery(stage: .done, irrevocable: true, nextAt: nil), height: 1_000).says, "3,000 sat back on Bitcoin")
        XCTAssertTrue(forcing(recovery: recovery(stage: .lost, irrevocable: true), height: 1_000).failed)
        // Closed with the hub, its fold awaited: signed, so not to be given up.
        let closing = forcing(recovery: recovery(stage: .folding, irrevocable: true), height: 1_000)
        XCTAssertEqual(closing.stands, "Closing")
        XCTAssertFalse(closing.revocable || closing.failed)
    }

    func test_canceling_requires_an_unsigned_claim_and_never_a_later_stage() {
        let all: [RecoveryStage] = [
            .folding, .claimed, .requesting, .requested, .timingOut, .sweeping, .done, .answered, .lost, .asserted,
            .challenging, .challenged, .paid,
        ]
        for stage in all {
            XCTAssertEqual(forcing(recovery: recovery(stage: stage), height: 0).revocable, stage == .claimed)
            XCTAssertFalse(forcing(recovery: recovery(stage: stage, irrevocable: true), height: 0).revocable)
        }
    }

    func test_a_forced_close_the_hub_answered_awaits_the_hubs_proof_by_its_deadline() {
        let answered = recovery(stage: .answered, irrevocable: true, nextAt: nil)
        XCTAssertEqual(forcing(recovery: answered, height: 1_000).says, "The hub answered")
        XCTAssertFalse(forcing(recovery: answered, height: 1_000).failed)
        let owed = { var c = answered; c.deadline = 1_144; c.corrected = 1_000; return c }()
        XCTAssertEqual(forcing(recovery: owed, height: 1_000).stands, "Answered")
        XCTAssertEqual(
            forcing(recovery: owed, height: 1_000).says,
            "The hub says 1,000 sat is owed: it must prove so within 24 hours, or you are paid your claim"
        )
        let asserted = forcing(recovery: { var c = recovery(stage: .asserted, irrevocable: true); c.corrected = 1_000; return c }(), height: 1_000)
        XCTAssertEqual(asserted.says, "The hub says 1,000 sat is owed: paid in 24 hours")
        XCTAssertEqual(forcing(recovery: recovery(stage: .challenging, irrevocable: true), height: 1_000).stands, "Challenging")
        XCTAssertEqual(forcing(recovery: recovery(stage: .paid, irrevocable: true), height: 1_000).says, "Back on Bitcoin in 24 hours")
    }

    func test_a_payment_link_says_its_amount_in_bitcoin() {
        XCTAssertEqual(btc(amount: 33_000), "0.00033000")
        XCTAssertEqual(btc(amount: 100_000_001), "1.00000001")
    }
}
