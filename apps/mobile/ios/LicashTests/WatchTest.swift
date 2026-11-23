import XCTest
@testable import Licash
import LicashCore

final class WatchTest: XCTestCase {
    private let now: Int64 = 1_800_000_000

    private func line(kind: Kind, state: State, id: String = "a", amount: UInt64 = 1_000, at: Int64? = nil) -> Activity {
        Activity(id: id, kind: kind, state: state, amount: amount, fee: 0, detail: "the hub refused", at: at)
    }

    private func steps(coin: String? = "c0", claim: String? = nil, claimBy: UInt32? = 1_000, tip: UInt32 = 900) -> PayoutSteps {
        PayoutSteps(
            lock: .locked, lockExpiry: 1_100, timeout: 1_010, claimBy: claimBy, coin: coin,
            coinHeight: nil, claim: claim, claimHeight: nil, depth: 1, tip: tip
        )
    }

    private func payout(steps: PayoutSteps) -> Details {
        .payout(
            hub: "https://hub.example", provider: "https://swaps.example", address: "bc1q", coin: steps.coin,
            claim: steps.claim, failure: nil, progress: steps, providerFee: 0, claimFee: 0, lockFee: 0
        )
    }

    private func deposit(paid: Bool) -> Details {
        .moved(
            hub: "https://hub.example", hash: "h", pays: nil, address: "bc1q",
            payment: paid ? Onchain(txid: "t", fee: nil, height: nil, confirmations: 0) : nil,
            depth: nil, fundBy: nil, timeout: nil, refund: nil
        )
    }

    func test_nothing_under_way_is_not_watched() {
        let ui = Ui(history: [line(kind: .sent, state: .done), line(kind: .received, state: .done, id: "b")])
        XCTAssertEqual(watchOf(ui: ui, now: now), Watch.none)
    }

    func test_a_payment_under_way_is_watched_now() {
        for kind in [Kind.payout, .swap, .invoice, .lightning, .sent, .onchain] {
            XCTAssertEqual(watchOf(ui: Ui(history: [line(kind: kind, state: .pending)]), now: now), Watch.now, "\(kind)")
        }
    }

    func test_a_deposit_never_sent_is_looked_at_later_but_once_sent_or_unknown_now() {
        let moved = line(kind: .moved, state: .pending)
        XCTAssertEqual(watchOf(ui: Ui(history: [moved], under: [moved.key: deposit(paid: false)]), now: now), Watch.later)
        XCTAssertEqual(watchOf(ui: Ui(history: [moved], under: [moved.key: deposit(paid: true)]), now: now), Watch.now)
        // Where it stands unread (past the first lines under way): perhaps sent.
        XCTAssertEqual(watchOf(ui: Ui(history: [moved]), now: now), Watch.now)
    }

    func test_note_work_waiting_for_privacy_rest_is_looked_at_later_until_done() {
        // A receive-only wallet: everything finished, notes resting.
        let received = [line(kind: .received, state: .done)]
        XCTAssertEqual(watchOf(ui: Ui(history: received, noteMaintenance: true), now: now), Watch.later)
        XCTAssertEqual(watchOf(ui: Ui(history: received, noteMaintenance: false), now: now), Watch.none)
    }

    func test_an_optimization_under_way_is_looked_at_later_never_kept_at_work() {
        let optimizing = line(kind: .optimized, state: .pending, amount: 0)
        XCTAssertEqual(watchOf(ui: Ui(history: [optimizing]), now: now), Watch.later)
        // A real payment beside it still keeps the app at work.
        let paying = line(kind: .sent, state: .pending, id: "b")
        XCTAssertEqual(watchOf(ui: Ui(history: [optimizing, paying], noteMaintenance: true), now: now), Watch.now)
    }

    func test_a_payment_on_its_way_to_its_payee_is_looked_at_later() {
        XCTAssertEqual(watchOf(ui: Ui(onTheirWay: 1), now: now), Watch.later)
        XCTAssertEqual(watchOf(ui: Ui(onTheirWay: 0), now: now), Watch.none)
    }

    func test_a_contract_being_opened_is_watched_now() {
        XCTAssertEqual(watchOf(ui: Ui(opening: "https://hub.example"), now: now), Watch.now)
    }

    func test_a_line_under_way_for_over_a_week_is_stuck_not_watched() {
        let stuck = line(kind: .payout, state: .pending, at: now - STALE_SECS)
        XCTAssertEqual(watchOf(ui: Ui(history: [stuck]), now: now), Watch.none)
        let recent = line(kind: .payout, state: .pending, at: now - STALE_SECS + 60)
        XCTAssertEqual(watchOf(ui: Ui(history: [recent]), now: now), Watch.now)
    }

    func test_a_contract_forced_closed_is_looked_at_later_until_over() {
        func forced(stage: RecoveryStage) -> Recovery {
            Recovery(
                id: "c0", stage: stage, irrevocable: true, claimed: 3_000, notes: 7, request: nil,
                publication: nil, timeout: nil, sweep: nil, nextAt: nil, error: nil
            )
        }
        for stage in [RecoveryStage.claimed, .folding, .requested, .sweeping] {
            XCTAssertEqual(watchOf(ui: Ui(recoveries: [forced(stage: stage)]), now: now), Watch.later, "\(stage)")
        }
        for stage in [RecoveryStage.done, .lost] {
            XCTAssertEqual(watchOf(ui: Ui(recoveries: [forced(stage: stage)]), now: now), Watch.none, "\(stage)")
        }
    }

    func test_a_contract_protecting_is_looked_at_later_for_its_exit() {
        func contract(state: ContractState) -> Contract {
            Contract(
                id: "c0", hub: "https://hub.example", state: state, mock: false, cap: 4_096, covers: 3_000,
                price: 1_000, funded: 0, fundedReceived: false, paidFrom: 0, protectsUntil: 5_000, closesAt: 5_500, funding: nil,
                createdAt: 0, step: nil, transfer: nil
            )
        }
        XCTAssertEqual(watchOf(ui: Ui(contracts: [contract(state: .protecting)]), now: now), Watch.later)
        for state in [ContractState.ending, .closed] {
            XCTAssertEqual(watchOf(ui: Ui(contracts: [contract(state: state)]), now: now), Watch.none)
        }
    }

    func test_a_contract_is_carried_on_until_open_and_her_funded_notes_received() {
        func contract(state: ContractState, funded: UInt64 = 0, received: Bool = false) -> Contract {
            Contract(
                id: "c0", hub: "https://hub.example", state: state, mock: false, cap: 131_071, covers: 0,
                price: 23_024, funded: funded, fundedReceived: received, paidFrom: 23_300, protectsUntil: 5_000,
                closesAt: 5_500, funding: nil, createdAt: 0, step: nil, transfer: nil
            )
        }
        XCTAssertTrue(carriedOn(contract: contract(state: .opening, funded: 276)))
        XCTAssertTrue(carriedOn(contract: contract(state: .confirming, funded: 276)))
        // Her notes are issued once its funding is buried, by then protecting.
        XCTAssertTrue(carriedOn(contract: contract(state: .protecting, funded: 276)))
        XCTAssertFalse(carriedOn(contract: contract(state: .protecting, funded: 276, received: true)))
        XCTAssertFalse(carriedOn(contract: contract(state: .protecting)))
        for state in [ContractState.ending, .closed] {
            XCTAssertFalse(carriedOn(contract: contract(state: state, funded: 276)), "\(state)")
        }
    }

    func test_an_exit_tells_what_it_did_and_what_she_must_do_never_where() {
        func exit(step: ExitStep, error: String? = nil, id: String = "c0") -> ContractExit {
            ContractExit(
                id: id, hub: "https://hub.example", step: step, cashAt: 900, forceAt: 1_188,
                uncovered: 3_000, error: error
            )
        }
        let quiet = [ExitStep.waiting, .covered, .paying].map { exit(step: $0) }
        XCTAssertTrue(exitNotices(exits: quiet, recoveries: []).isEmpty)
        // Retried each round until done: told once it is.
        XCTAssertTrue(exitNotices(exits: [exit(step: .cashingOut, error: "no provider")], recoveries: []).isEmpty)
        let told = exitNotices(
            exits: [
                exit(step: .needsConsent, id: "a"),
                exit(step: .cashingOut, id: "b"),
                exit(step: .forceClosing, error: "no fee coin", id: "c"),
            ],
            recoveries: []
        )
        XCTAssertEqual(told.map(\.key), ["exit a NEEDS_CONSENT", "exit b CASHING_OUT", "exit c FORCE_CLOSING failed"])
        XCTAssertEqual(told.map(\.urgent), [true, false, true])
        XCTAssertTrue(told[0].text.contains("3,000 sat") && told[0].text.contains("Allow exits"), told[0].text)
        XCTAssertTrue(told.allSatisfy { !$0.text.contains("hub.example") })
        // Locks owed to her only its forced close recovers: told apart.
        let owed = exitNotices(exits: [{ var e = exit(step: .needsConsent); e.owed = 1_000; return e }()], recoveries: [])
        XCTAssertTrue(owed[0].text.contains("1,000 sat") && owed[0].text.contains("forcing it closed"), owed[0].text)
    }

    func test_a_forced_close_held_back_by_its_fee_asks_her_to_allow_more() {
        func forced(capped: Bool, stage: RecoveryStage = .requesting) -> Recovery {
            Recovery(
                id: "c0", stage: stage, irrevocable: true, claimed: 3_000, notes: 7, request: nil,
                publication: nil, timeout: nil, sweep: nil, nextAt: nil, error: nil, feeCapped: capped
            )
        }
        XCTAssertTrue(exitNotices(exits: [], recoveries: [forced(capped: false)]).isEmpty)
        XCTAssertTrue(exitNotices(exits: [], recoveries: [forced(capped: true, stage: .lost)]).isEmpty)
        let told = exitNotices(exits: [], recoveries: [forced(capped: true)])
        XCTAssertEqual(told.map(\.key), ["fee c0"])
        XCTAssertTrue(told[0].urgent && told[0].text.contains("allow it"))
    }

    func test_a_disputed_forced_close_asks_her_to_stay_online() {
        func forced(stage: RecoveryStage) -> Recovery {
            Recovery(
                id: "c0", stage: stage, irrevocable: true, claimed: 3_000, notes: 7, request: nil,
                publication: nil, timeout: nil, sweep: nil, nextAt: nil, error: nil, deadline: 2_000
            )
        }
        let told = exitNotices(exits: [], recoveries: [forced(stage: .asserted)])
        XCTAssertEqual(told.map(\.key), ["dispute c0"])
        XCTAssertTrue(told[0].urgent && told[0].text.contains("online"))
        XCTAssertTrue(exitNotices(exits: [], recoveries: [forced(stage: .requested)]).isEmpty)
    }

    func test_a_hub_answer_keeps_the_app_at_work_until_paid() {
        func forced(stage: RecoveryStage) -> Recovery {
            Recovery(
                id: "c0", stage: stage, irrevocable: true, claimed: 3_000, notes: 7, request: nil,
                publication: nil, timeout: nil, sweep: nil, nextAt: nil, error: nil, deadline: 2_000
            )
        }
        for stage in [
            RecoveryStage.answered,
            .asserted,
            .challenging,
            .challenged,
        ] {
            XCTAssertEqual(watchOf(ui: Ui(recoveries: [forced(stage: stage)]), now: now), Watch.now, "\(stage)")
        }
        for stage in [RecoveryStage.paid] {
            XCTAssertEqual(watchOf(ui: Ui(recoveries: [forced(stage: stage)]), now: now), Watch.later, "\(stage)")
        }
    }

    func test_a_lightning_invoice_asked_is_looked_for_later_for_a_day() {
        XCTAssertEqual(watchOf(ui: Ui(asked: now - ASKED_SECS + 60), now: now), Watch.later)
        XCTAssertEqual(watchOf(ui: Ui(asked: now - ASKED_SECS), now: now), Watch.none)
    }

    func test_an_invoice_paid_at_once_is_told_if_not_seen_before_in_this_run() {
        let paid = line(kind: .invoice, state: .done, id: "i")
        let old = line(kind: .payout, state: .done, id: "o")
        // Seen nothing yet since the app started: nothing new to tell.
        XCTAssertTrue(look(history: [paid, old], before: [], seen: nil).finished.isEmpty)
        let look = look(history: [paid, old], before: [], seen: [old.key])
        XCTAssertEqual(look.finished, [paid])
        XCTAssertTrue(Licash.look(history: [paid, old], before: [], seen: [old.key, paid.key]).finished.isEmpty)
        // Never looked before (an upgraded app): nothing.
        XCTAssertTrue(Licash.look(history: [paid, old], before: nil, seen: [old.key]).finished.isEmpty)
    }

    func test_never_looked_before_nothing_is_told_finished() {
        let history = [line(kind: .payout, state: .done, id: "old"), line(kind: .swap, state: .pending, id: "now")]
        let look = look(history: history, before: nil)
        XCTAssertTrue(look.finished.isEmpty)
        XCTAssertEqual(look.under, ["\(Kind.swap) now"])
    }

    func test_a_line_under_way_when_last_looked_is_told_once_finished() {
        let before = look(history: [line(kind: .payout, state: .pending, id: "p"), line(kind: .swap, state: .pending, id: "s")], before: nil)
        let after = [line(kind: .payout, state: .done, id: "p"), line(kind: .swap, state: .pending, id: "s")]
        let look = look(history: after, before: before.under)
        XCTAssertEqual(look.finished.map(\.id), ["p"])
        XCTAssertEqual(look.under, ["\(Kind.swap) s"])
        // Not again.
        XCTAssertTrue(Licash.look(history: after, before: look.under).finished.isEmpty)
    }

    func test_lines_of_other_kinds_sharing_an_id_are_apart() {
        let before: Set<String> = ["\(Kind.swap) x"]
        let look = look(history: [line(kind: .payout, state: .done, id: "x"), line(kind: .swap, state: .pending, id: "x")], before: before)
        XCTAssertTrue(look.finished.isEmpty)
    }

    func test_finished_lines_are_told_in_words_failures_with_why() {
        XCTAssertEqual(news(activity: line(kind: .payout, state: .done, amount: 21_852)), "Sent 21,852 sat on Bitcoin")
        XCTAssertEqual(
            news(activity: line(kind: .swap, state: .failed)),
            "Moving 1,000 sat between hubs failed: open Licash for why"
        )
        XCTAssertNil(news(activity: line(kind: .payout, state: .pending)))
        // A coin moving in shows as its deposit, told of instead.
        XCTAssertNil(news(activity: line(kind: .onchain, state: .done)))
    }

    func test_payout_deadlines_are_read_beyond_the_ordinary_progress_limit() {
        let ordinary = (1...25).map { line(kind: .swap, state: .pending, id: "s\($0)") }
        let p = line(kind: .payout, state: .pending, id: "p")
        let done = line(kind: .payout, state: .done, id: "done")
        let read = detailsToRead(history: ordinary + [p, done])
        XCTAssertEqual(read, Array(ordinary.prefix(20)) + [p])
        let under = Dictionary(uniqueKeysWithValues: read.filter { $0.kind == .payout }.map { ($0.key, payout(steps: steps(tip: 999))) })
        XCTAssertEqual(nearing(ui: Ui(history: ordinary + [p, done], under: under)), [p.key: 1_000])
    }

    func test_a_payout_is_warned_of_only_with_the_providers_coin_unclaimed_near_its_deadline() {
        let p = line(kind: .payout, state: .pending, id: "p")
        func near(steps: PayoutSteps, height: Int = 0) -> [String: UInt32] {
            nearing(ui: Ui(history: [p], chain: ChainStatus(height: height), under: [p.key: payout(steps: steps)]))
        }
        XCTAssertEqual(near(steps: steps(tip: 1_000 - WARN_BLOCKS)), [p.key: 1_000])
        XCTAssertEqual(near(steps: steps(tip: 1_000 - WARN_BLOCKS - 1)), [String: UInt32]())
        // The light client further than the wallet last looked.
        XCTAssertEqual(near(steps: steps(tip: 900), height: 995), [p.key: 1_000])
        XCTAssertEqual(near(steps: steps(coin: nil, tip: 999)), [String: UInt32]())
        XCTAssertEqual(near(steps: steps(claim: "c1", tip: 999)), [String: UInt32]())
        XCTAssertEqual(near(steps: steps(claimBy: nil, tip: 999)), [String: UInt32]())
    }
}
