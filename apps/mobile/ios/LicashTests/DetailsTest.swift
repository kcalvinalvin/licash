import LicashCore
import XCTest
@testable import Licash

class DetailsTest: XCTestCase {
    private func line(kind: Kind, state: LicashCore.State = .pending) -> Activity {
        Activity(id: "a", kind: kind, state: state, amount: 1_000, fee: 0, detail: "The hub refused", at: nil)
    }

    private func steps(
        lock: LockStep? = .locked,
        coin: String? = "c0",
        claim: String? = nil,
        claimBy: UInt32? = 1_000,
        tip: UInt32 = 900
    ) -> PayoutSteps {
        PayoutSteps(
            lock: lock, lockExpiry: 1_100, timeout: 1_010, claimBy: claimBy, coin: coin, coinHeight: nil,
            claim: claim, claimHeight: nil, depth: 1, tip: tip
        )
    }

    private func payout(steps: PayoutSteps) -> Details {
        .payout(
            hub: "https://hub.example", provider: "https://swaps.example", address: "bc1q", coin: steps.coin,
            claim: steps.claim, failure: nil, progress: steps, providerFee: 0, claimFee: 0, lockFee: 0
        )
    }

    private func deposit(confirmations: UInt32?, depth: UInt32? = 2) -> Details {
        .moved(
            hub: "https://hub.example", hash: "h", pays: nil, address: "bc1q",
            payment: confirmations.map { Onchain(txid: "t", fee: nil, height: nil, confirmations: $0) },
            depth: depth, fundBy: nil, timeout: nil, refund: nil
        )
    }

    private func swap(lock: String?) -> Details {
        .swap(
            from: "https://a.example", to: "https://b.example", provider: "https://swaps.example", lock: lock,
            theirs: nil, failure: nil, providerFee: 0, lockFee: 0
        )
    }

    func test_a_payout_is_on_its_way_then_sent_then_arrived() {
        let under = line(kind: .payout)
        XCTAssertEqual(status(activity: under, coming: false, details: payout(steps: steps(coin: nil))), "On its way")
        XCTAssertEqual(status(activity: under, coming: false, details: payout(steps: steps())), "On its way")
        XCTAssertEqual(
            status(activity: under, coming: false, details: payout(steps: steps(claim: "t"))),
            "Sent. Confirming, about 10 min"
        )
        XCTAssertEqual(
            status(activity: line(kind: .payout, state: .done), coming: false, details: payout(steps: steps(claim: "t"))),
            "Arrived"
        )
    }

    func test_one_that_failed_says_why_and_what_becomes_of_the_money() {
        let failed = line(kind: .payout, state: .failed)
        XCTAssertEqual(
            status(activity: failed, coming: false, details: payout(steps: steps())),
            "The hub refused. Your money comes back by itself."
        )
        XCTAssertEqual(
            status(activity: failed, coming: false, details: payout(steps: steps(lock: nil))),
            "The hub refused. Nothing was taken."
        )
        let moved = line(kind: .swap, state: .failed)
        XCTAssertEqual(status(activity: moved, coming: false, details: swap(lock: nil)), "The hub refused. Nothing was taken.")
        XCTAssertEqual(
            status(activity: moved, coming: false, details: swap(lock: "l")),
            "The hub refused. Your money comes back by itself."
        )
        XCTAssertEqual(status(activity: line(kind: .sent, state: .failed), coming: false, details: nil), "The hub refused")
    }

    func test_bitcoin_coming_in_counts_its_blocks_down_as_time() {
        let moved = line(kind: .moved)
        XCTAssertEqual(status(activity: line(kind: .onchain), coming: true, details: nil), "Confirming, about 10 min")
        XCTAssertEqual(status(activity: moved, coming: false, details: deposit(confirmations: nil)), "Waiting for its payment")
        XCTAssertEqual(status(activity: moved, coming: false, details: deposit(confirmations: 0)), "Confirming, about 20 min")
        XCTAssertEqual(status(activity: moved, coming: false, details: deposit(confirmations: 1)), "Confirming, about 10 min")
        XCTAssertEqual(status(activity: moved, coming: false, details: deposit(confirmations: 2)), "Almost there")
        XCTAssertEqual(
            status(activity: line(kind: .moved, state: .done), coming: false, details: deposit(confirmations: 2)),
            "In your balance"
        )
    }

    func test_no_status_names_a_block_a_note_or_a_provider() {
        let all = [
            status(activity: line(kind: .payout), coming: false, details: payout(steps: steps())),
            status(activity: line(kind: .payout, state: .failed), coming: false, details: payout(steps: steps())),
            status(activity: line(kind: .swap), coming: false, details: swap(lock: "l")),
            status(activity: line(kind: .swap, state: .done), coming: false, details: swap(lock: "l")),
            status(activity: line(kind: .moved), coming: false, details: deposit(confirmations: 0)),
            status(activity: line(kind: .contract), coming: false, details: .contract(hub: "https://hub.example", funding: "f")),
            status(
                activity: line(kind: .contract, state: .done), coming: false,
                details: .contract(hub: "https://hub.example", funding: "f")
            ),
            status(
                activity: line(kind: .invoice), coming: false,
                details: .invoice(hub: "https://hub.example", invoice: nil, asks: nil, failure: nil)
            ),
            status(activity: line(kind: .sent), coming: false, details: nil),
        ]
        for said in all {
            for word in ["block", "note", "provider", "contract", "Licash"] {
                XCTAssertFalse(said.lowercased().contains(word.lowercased()), "\(said): \(word)")
            }
        }
    }

    func test_a_payout_to_finish_asks_for_the_app_in_time_not_blocks() {
        let under = line(kind: .payout)
        XCTAssertEqual(needs(activity: under, p: steps(), developer: false), "Open the app within 17 hours to finish it")
        XCTAssertEqual(
            needs(activity: under, p: steps(), developer: true),
            "Open the app within 17 hours to finish it (block 1000)"
        )
        XCTAssertEqual(needs(activity: under, p: steps(tip: 0), developer: false), "Keep the app open to finish it")
        XCTAssertEqual(
            needs(activity: under, p: steps(tip: 1_001), developer: false),
            "Too late to finish: your money comes back by itself"
        )
        // Nothing asked once sent, once over, or with no time to run out.
        XCTAssertNil(needs(activity: under, p: steps(claim: "t"), developer: false))
        XCTAssertNil(needs(activity: line(kind: .payout, state: .done), p: steps(), developer: false))
        XCTAssertNil(needs(activity: under, p: steps(claimBy: nil), developer: false))
    }

    func test_the_history_names_a_line_in_a_word_or_two() {
        XCTAssertEqual(title(activity: line(kind: .onchain), coming: true), "Arriving")
        XCTAssertEqual(title(activity: line(kind: .contract), coming: false), "Protection")
        XCTAssertEqual(title(activity: line(kind: .payout), coming: false), "Sent on Bitcoin")
        XCTAssertEqual(title(activity: line(kind: .moved), coming: false), "From Bitcoin")
        // Her part of the coin a contract took, back as notes: not a payment from anyone.
        XCTAssertEqual(title(activity: line(kind: .funded), coming: false), "From Bitcoin, protected")
        XCTAssertFalse(paid(activity: line(kind: .funded)))
    }

    func test_a_contract_says_what_it_cost_apart_from_the_reserve_it_gives_back_as_it_closes() {
        // Its money back is said as such, on its way too, and paid in, not out.
        XCTAssertEqual(title(activity: line(kind: .closed), coming: false), "Protection closed")
        XCTAssertEqual(title(activity: line(kind: .closed), coming: true), "Protection closed")
        XCTAssertFalse(paid(activity: line(kind: .closed)))
        XCTAssertTrue(paid(activity: line(kind: .contract)))
        XCTAssertEqual(opened(price: 23_655, reserve: 20_000), 3_655)
        XCTAssertEqual(opened(price: 5_000, reserve: 20_000), 0)
    }

    private func lightning(provider: String?, failure: String?, refundBlocks: UInt32? = nil) -> Details {
        .lightning(
            hub: "https://hub.example", invoice: "lnbc", preimage: nil, failure: failure, provider: provider,
            expiry: provider.map { _ in 1_200 }, refundBlocks: refundBlocks
        )
    }

    func test_a_payment_a_swap_provider_did_not_make_waits_for_its_lock_to_expire() {
        let under = line(kind: .lightning)
        XCTAssertEqual(
            status(activity: under, coming: false, details: lightning(provider: "https://swaps.example", failure: nil, refundBlocks: 200)),
            "Under way"
        )
        XCTAssertEqual(
            status(
                activity: under, coming: false,
                details: lightning(provider: "https://swaps.example", failure: "declined", refundBlocks: 200)
            ),
            "Not paid. Your money comes back by itself in about 33 hours."
        )
        XCTAssertEqual(
            status(activity: under, coming: false, details: lightning(provider: "https://swaps.example", failure: "declined")),
            "Not paid. Your money comes back by itself."
        )
        // A hub's gateway refunds a failed payment at once: never this.
        XCTAssertEqual(status(activity: under, coming: false, details: lightning(provider: nil, failure: "no route")), "Under way")
    }
}
