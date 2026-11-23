import LicashCore
import XCTest
@testable import Licash

class GlanceTest: XCTestCase {
    private let now: Int64 = 1_800_000_000
    private let synced = ChainStatus(height: 900, peers: 1, synced: true)

    private func line(kind: Kind, state: LicashCore.State = .pending, id: String = "a", at: Int64? = nil) -> Activity {
        Activity(id: id, kind: kind, state: state, amount: 10_000, fee: 0, detail: "", at: at)
    }

    private func hub() -> Hub {
        Hub(
            url: "https://hub.example", feeBase: 0, feePpm: 0, feeOutputMsat: 0, deposits: nil, silent: false,
            mailbox: false,
            lightningPay: nil, lockFee: Rate(base: 0, ppm: 0), refundFee: Rate(base: 0, ppm: 0), depositFee: nil,
            lightning: nil, contracts: nil
        )
    }

    private func deposit(paid: Bool) -> Details {
        .moved(
            hub: "https://hub.example", hash: "h", pays: nil, address: "bc1q",
            payment: paid ? Onchain(txid: "t", fee: nil, height: nil, confirmations: 0) : nil,
            depth: nil, fundBy: nil, timeout: nil, refund: nil
        )
    }

    func test_at_rest_home_says_nothing() {
        let done = [line(kind: .sent, state: .done), line(kind: .received, state: .failed, id: "b")]
        XCTAssertNil(glance(ui: Ui(history: done, hubs: [hub()], chain: synced), now: now))
        // No hub yet: nothing to catch up for.
        XCTAssertNil(glance(ui: Ui(), now: now))
    }

    func test_one_payment_under_way_is_told_with_its_amount_and_opens_itself() {
        let coming = line(kind: .onchain)
        guard case let .one(text, activity, isComing)? = glance(ui: Ui(chain: synced, incoming: [coming]), now: now) else {
            return XCTFail("not one payment")
        }
        XCTAssertEqual(text, "Receiving 10,000 sat")
        XCTAssertEqual(activity, coming)
        XCTAssertEqual(isComing, true)

        let texts: [Kind: String] = [
            .sent: "Sending 10,000 sat",
            .lightning: "Sending 10,000 sat",
            .payout: "Sending 10,000 sat",
            .received: "Receiving 10,000 sat",
            .invoice: "Receiving 10,000 sat",
            .moved: "Receiving 10,000 sat",
            .funded: "Receiving 10,000 sat",
            .onchain: "Receiving 10,000 sat",
            .swap: "Moving 10,000 sat",
            .contract: "Setting up protection",
            .optimized: "Optimizing notes",
            .closed: "Receiving 10,000 sat",
        ]
        let entries: Set<Kind> = [
            .sent, .received, .moved, .onchain, .lightning, .invoice, .payout, .contract, .swap, .optimized, .funded, .closed,
        ]
        XCTAssertEqual(Set(texts.keys), entries)
        for (kind, text) in texts {
            guard case let .one(said, _, under)? = glance(ui: Ui(history: [line(kind: kind)], chain: synced), now: now) else {
                XCTFail("\(kind)")
                continue
            }
            XCTAssertEqual(said, text, "\(kind)")
            XCTAssertEqual(under, false)
        }
    }

    func test_several_under_way_are_counted_and_open_the_history() {
        let ui = Ui(
            history: [line(kind: .sent), line(kind: .payout, id: "b"), line(kind: .sent, state: .done, id: "d")],
            chain: synced,
            incoming: [line(kind: .onchain, id: "c")]
        )
        let many = glance(ui: ui, now: now)
        XCTAssertEqual(many, .many(count: 3))
        XCTAssertEqual(many?.text, "3 in progress")
    }

    func test_what_is_stuck_or_never_paid_is_left_to_the_history() {
        let stuck = line(kind: .sent, at: now - STALE_SECS - 1)
        XCTAssertNil(glance(ui: Ui(history: [stuck], chain: synced), now: now))
        let recent = line(kind: .sent, at: now - 60)
        XCTAssertEqual(glance(ui: Ui(history: [recent], chain: synced), now: now)?.text, "Sending 10,000 sat")

        let moved = line(kind: .moved)
        XCTAssertNil(glance(ui: Ui(history: [moved], chain: synced, under: [moved.key: deposit(paid: false)]), now: now))
        let paid = Ui(history: [moved], chain: synced, under: [moved.key: deposit(paid: true)])
        XCTAssertEqual(glance(ui: paid, now: now)?.text, "Receiving 10,000 sat")
    }

    func test_a_payment_comes_before_protection_then_a_problem() {
        let all = Ui(
            history: [line(kind: .sent)], hubs: [hub()], problem: "No hub answered", opening: "https://hub.example"
        )
        XCTAssertEqual(glance(ui: all, now: now)?.text, "Sending 10,000 sat")
        let opening = { var ui = all; ui.history = []; return ui }()
        XCTAssertEqual(glance(ui: opening, now: now), .protecting(hub: "https://hub.example"))
        XCTAssertEqual(glance(ui: opening, now: now)?.text, "Setting up protection")
        let behind = { var ui = opening; ui.opening = nil; return ui }()
        XCTAssertEqual(glance(ui: behind, now: now), .behind(reason: "No hub answered"))
        XCTAssertEqual(glance(ui: behind, now: now)?.text, "Not up to date")
        // Catching up is the connections' to say, not this line's.
        let syncing = { var ui = behind; ui.problem = nil; return ui }()
        XCTAssertNil(glance(ui: syncing, now: now))
        XCTAssertNil(glance(ui: { var ui = syncing; ui.chain = synced; return ui }(), now: now))
    }
}
