import LicashCore
import XCTest
@testable import Licash

class WordsTest: XCTestCase {
    private func balance(notes: UInt64 = 0, onchain: UInt64 = 0, covered: UInt64 = 0, pending: UInt64 = 0) -> Balance {
        Balance(
            notes: notes, notesPending: pending, onchain: onchain, moving: 0, kept: onchain, opening: 0,
            covered: covered
        )
    }

    func test_a_payment_with_too_little_is_sent_where_its_bitcoin_or_nothing_is() {
        // A contract offered comes first, as before: too little, or nothing held, whatever else failed.
        XCTAssertEqual(lack(insufficient: true, noHub: false, notes: 5, bitcoin: 0, offered: true), .offer)
        XCTAssertEqual(lack(insufficient: false, noHub: false, notes: 0, bitcoin: 9, offered: true), .offer)
        // No offer, nothing in notes, too little: bitcoin held or coming is where to look, else receive first.
        XCTAssertEqual(lack(insufficient: false, noHub: true, notes: 0, bitcoin: 100, offered: false), .bitcoin)
        XCTAssertEqual(lack(insufficient: false, noHub: true, notes: 0, bitcoin: 0, offered: false), .nothing)
        XCTAssertEqual(lack(insufficient: true, noHub: false, notes: 0, bitcoin: 0, offered: false), .nothing)
        // Notes held somewhere else, or a failure that is not about a balance: said as it is.
        XCTAssertEqual(lack(insufficient: false, noHub: true, notes: 40, bitcoin: 100, offered: false), .other)
        XCTAssertEqual(lack(insufficient: true, noHub: false, notes: 40, bitcoin: 0, offered: false), .other)
        XCTAssertEqual(lack(insufficient: false, noHub: false, notes: 0, bitcoin: 100, offered: false), .other)
        // Too little in notes, bitcoin held besides: adding it mends that, notes held or not.
        XCTAssertEqual(lack(insufficient: true, noHub: false, notes: 21_000, bitcoin: 110_000, offered: false), .bitcoin)
    }

    func test_a_coin_on_bitcoin_is_added_at_once_unless_it_is_still_on_its_way() {
        XCTAssertTrue(addable(balance: balance(onchain: 10_000), opening: false))
        // Being added already, paying for protection, or not on Bitcoin yet: only waited for.
        XCTAssertFalse(addable(balance: { var c = balance(onchain: 10_000); c.moving = 5_000; return c }(), opening: false))
        XCTAssertFalse(addable(balance: balance(onchain: 10_000), opening: true))
        XCTAssertFalse(addable(balance: balance(), opening: false))
        XCTAssertTrue(addFirst(addable: false, offered: true).contains("on its way"))
        XCTAssertTrue(addFirst(addable: true, offered: true).contains("protect it first"))
        XCTAssertFalse(addFirst(addable: true, offered: false).contains("protect it first"))
    }

    func test_a_payment_says_the_contact_it_went_to_or_came_from() {
        XCTAssertEqual(counterpart(kind: .sent, contact: "Bob"), "To Bob")
        XCTAssertEqual(counterpart(kind: .received, contact: "Alice"), "From Alice")
        XCTAssertNil(counterpart(kind: .sent, contact: nil))
    }

    func test_home_never_says_protected_while_protection_is_being_set_up() {
        let held = balance(notes: 10_000, covered: 10_000)
        XCTAssertEqual(homeProtection(balance: held, opening: false), protection(balance: held))
        XCTAssertNil(homeProtection(balance: held, opening: true))
    }

    func test_bitcoin_paid_to_the_wallet_with_nothing_done_about_it_is_said_waiting() {
        // Paid on Bitcoin, no offer for it, nothing under way: waiting, less what the wallet keeps on purpose.
        XCTAssertEqual(waiting(balance: { var c = balance(onchain: 10_000); c.kept = 0; return c }(), offered: false, opening: false), 10_000)
        XCTAssertEqual(waiting(balance: { var c = balance(onchain: 10_000); c.kept = 4_000; return c }(), offered: false, opening: false), 6_000)
        // Its own change and payouts only, an offer, a protection being set up or a move under way: nothing to say.
        XCTAssertNil(waiting(balance: balance(onchain: 10_000), offered: false, opening: false))
        XCTAssertNil(waiting(balance: { var c = balance(onchain: 10_000); c.kept = 0; return c }(), offered: true, opening: false))
        XCTAssertNil(waiting(balance: { var c = balance(onchain: 10_000); c.kept = 0; return c }(), offered: false, opening: true))
        XCTAssertNil(waiting(balance: { var c = balance(onchain: 10_000); c.kept = 0; c.moving = 5_000; return c }(), offered: false, opening: false))
        XCTAssertNil(waiting(balance: balance(), offered: false, opening: false))
    }

    func test_nothing_held_says_nothing_of_protection() {
        let none = protection(balance: balance())
        XCTAssertNil(none.shield)
        XCTAssertNil(none.label)
        XCTAssertEqual(none.percent, 0)
    }

    func test_what_is_on_bitcoin_or_covered_is_protected_and_the_rest_is_not() {
        let p = protection(balance: balance(notes: 100_000, onchain: 10_000, covered: 4_096, pending: 9_294))
        XCTAssertEqual(p.protected, 14_096)
        XCTAssertEqual(p.open, 105_198)
        XCTAssertEqual(p.total, 119_294)
        XCTAssertEqual(p.shield, Shield.some)
        XCTAssertEqual(p.label, "12% protected")
    }

    func test_all_or_none_is_said_in_a_word_not_a_number() {
        let all = protection(balance: balance(notes: 4_000, onchain: 1_000, covered: 4_000))
        XCTAssertEqual(all.shield, Shield.all)
        XCTAssertEqual(all.label, "Protected")
        let none = protection(balance: balance(notes: 4_000))
        XCTAssertEqual(none.shield, Shield.none)
        XCTAssertEqual(none.label, "Not protected")
    }

    func test_a_share_is_never_rounded_to_all_or_none() {
        XCTAssertEqual(protection(balance: balance(notes: 1_000_000, onchain: 1)).percent, 1)
        XCTAssertEqual(protection(balance: balance(notes: 1, onchain: 1_000_000)).percent, 99)
        XCTAssertEqual(protection(balance: balance(notes: 500, onchain: 500)).percent, 50)
    }

    func test_covering_more_than_is_held_protects_no_more_than_is_held() {
        let p = protection(balance: balance(notes: 1_000, covered: 4_096))
        XCTAssertEqual(p.protected, 1_000)
        XCTAssertEqual(p.open, 0)
        XCTAssertEqual(p.shield, Shield.all)
    }

    func test_blocks_are_told_as_time_never_as_heights() {
        XCTAssertEqual(span(blocks: 0), "now")
        XCTAssertEqual(span(blocks: -3), "now")
        XCTAssertEqual(span(blocks: 1), "10 min")
        XCTAssertEqual(span(blocks: 5), "50 min")
        XCTAssertEqual(span(blocks: 6), "1 hour")
        XCTAssertEqual(span(blocks: 12), "2 hours")
        XCTAssertEqual(span(blocks: 144), "24 hours")
        XCTAssertEqual(span(blocks: 861), "6 days")
        XCTAssertEqual(span(blocks: 1_001), "7 days")
    }

    func test_blocks_are_told_in_the_time_of_their_network() {
        // Mutinynet's take 30 seconds: the same blocks, a twentieth of the time.
        XCTAssertEqual(span(blocks: 0, blockSecs: 30), "now")
        XCTAssertEqual(span(blocks: 1, blockSecs: 30), "30 sec")
        XCTAssertEqual(span(blocks: 10, blockSecs: 30), "5 min")
        XCTAssertEqual(span(blocks: 144, blockSecs: 30), "1 hour")
        XCTAssertEqual(span(blocks: 2_880, blockSecs: 30), "24 hours")
        XCTAssertEqual(span(blocks: 8_640, blockSecs: 30), "3 days")
        XCTAssertEqual(span(blocks: 1, blockSecs: 600), "10 min")
        XCTAssertEqual(span(blocks: 200, blockSecs: 600), "33 hours")
        XCTAssertEqual(
            refundWait(blocks: 200, blockSecs: 30),
            "If the provider declines or fails the payment, your sats come back by themselves, in about 2 hours."
        )
    }

    func test_a_day_is_said_as_short_as_today_lets_it() {
        let zone = TimeZone(identifier: "UTC")!
        let us = Locale(identifier: "en_US")
        // 2026-11-15 12:00 UTC.
        let now: Int64 = 1_794_744_000
        // Narrow or plain, the space before PM is the system's to choose.
        XCTAssertEqual(day(seconds: now - 3_600, now: now, zone: zone, locale: us).replacingOccurrences(of: "\u{202F}", with: " "), "11:00 AM")
        XCTAssertEqual(day(seconds: now - 86_400, now: now, zone: zone, locale: us), "Yesterday")
        XCTAssertEqual(day(seconds: now - 2 * 86_400, now: now, zone: zone, locale: us), "Nov 13")
        XCTAssertEqual(day(seconds: 1_767_182_400, now: now, zone: zone, locale: us), "Dec 31, 2025")
    }

    func test_paying_through_a_provider_says_how_long_its_sats_may_wait() {
        XCTAssertEqual(
            refundWait(blocks: 200),
            "If the provider declines or fails the payment, your sats come back by themselves, in about 33 hours."
        )
        XCTAssertEqual(notPaid(blocks: 12), "Not paid. Your money comes back by itself in about 2 hours.")
    }

    func test_a_setup_preparing_says_how_long_so_far_between_its_first_steps() {
        XCTAssertEqual(ticking(seconds: 45), "45 s")
        XCTAssertEqual(ticking(seconds: 125), "2 min 05 s")
        let (done, says) = setup(step: .preparing(seconds: 125))
        XCTAssertEqual(says, "Preparing it, a few minutes · 2 min 05 s")
        XCTAssertTrue(done > setup(step: .terms).0 && done < setup(step: .choosing).0)
    }

    func test_a_setup_says_how_much_it_sent_how_fast_and_how_long_the_rest_takes() {
        XCTAssertEqual(transferred(t: Transfer(bytes: 0, seconds: 0, expected: 100_100_000)), "0.0 MB of about 100 MB")
        XCTAssertEqual(
            transferred(t: Transfer(bytes: 12_000_000, seconds: 8, expected: 100_100_000)),
            "12 MB of about 100 MB · 1.5 MB/s · about 58 s left"
        )
        XCTAssertEqual(
            transferred(t: Transfer(bytes: 1_000_000, seconds: 10, expected: 100_100_000)),
            "1.0 MB of about 100 MB · 0.1 MB/s · about 17 min left"
        )
        XCTAssertEqual(
            transferred(t: Transfer(bytes: 99_000_000, seconds: 10, expected: 99_100_000)),
            "99 MB of about 99 MB · 9.9 MB/s · almost done"
        )
        XCTAssertEqual(
            transferred(t: Transfer(bytes: 1_000_000, seconds: 10, expected: 541_000_000)),
            "1.0 MB of about 541 MB · 0.1 MB/s · about 2 h left"
        )
        XCTAssertEqual(
            transferred(t: Transfer(bytes: 100_000, seconds: 48, expected: 251_000_000)),
            "0.1 MB of about 251 MB · 2 kB/s · about 33 h left"
        )
        // While the wallet prepares the contract itself, its slow start says nothing of the network.
        XCTAssertEqual(transferred(t: Transfer(bytes: 100_000, seconds: 48, expected: 251_000_000), busy: true), "0.1 MB of about 251 MB")
        // Past what was expected: no time left to say.
        XCTAssertEqual(transferred(t: Transfer(bytes: 120_000_000, seconds: 10, expected: 100_100_000)), "120 MB of about 100 MB · 12 MB/s")
    }

    func test_what_is_safe_is_told_apart_as_on_bitcoin_and_protected_by_contracts() {
        // 110,000 on Bitcoin, 9,800 in notes no contract covers: nothing by contracts.
        let share = Protection(protected: 110_000, open: 9_800)
        XCTAssertEqual(safeSplit(share: share, onBitcoin: 110_000), SafeSplit(bitcoin: 110_000, contracts: 0))
        // A contract covering notes besides.
        XCTAssertEqual(safeSplit(share: Protection(protected: 160_000, open: 0), onBitcoin: 110_000), SafeSplit(bitcoin: 110_000, contracts: 50_000))
        // Never more on Bitcoin than what is protected.
        XCTAssertEqual(safeSplit(share: Protection(protected: 5, open: 1), onBitcoin: 9), SafeSplit(bitcoin: 5, contracts: 0))
    }
}
