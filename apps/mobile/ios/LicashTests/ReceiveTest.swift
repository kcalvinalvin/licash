import LicashCore
import XCTest
@testable import Licash

class ReceiveTest: XCTestCase {
    func test_the_ways_to_be_paid_are_those_the_hubs_make_possible() {
        XCTAssertEqual(ways(lightning: false, silent: false), [.code, .bitcoin])
        XCTAssertEqual(ways(lightning: true, silent: false), [.lightning, .code, .bitcoin])
        XCTAssertEqual(ways(lightning: false, silent: true), [.code, .bitcoin, .silent])
        XCTAssertEqual(
            ways(lightning: true, silent: true).map { $0.label },
            ["Lightning invoice", "Licash code", "Bitcoin address", "Silent payment address"]
        )
    }

    func test_an_amount_is_asked_for_with_an_invoice_any_wallet_pays_where_a_hub_takes_lightning() {
        XCTAssertEqual(picked(amount: 500, lightning: true), .lightning)
        // Any amount, or no hub taking Lightning: the wallet's code.
        XCTAssertEqual(picked(amount: nil, lightning: true), .code)
        XCTAssertEqual(picked(amount: 500, lightning: false), .code)
    }

    func test_an_invoice_asks_the_amount_entered_and_says_what_the_wallet_gets() {
        // No fee: nothing to say.
        XCTAssertNil(invoiceWords(amount: 4_000, asks: 4_000, gets: 4_000))
        // The hub's fee in it.
        XCTAssertEqual(invoiceWords(amount: 4_000, asks: 4_000, gets: 3_997), "You get 3,997 sat, the hub's fee taken")
        // A sat short, where no amount and its fee come to it exactly.
        XCTAssertEqual(
            invoiceWords(amount: 100_002, asks: 100_001, gets: 100_000),
            "The payer pays 100,001 sat\nYou get 100,000 sat, the hub's fee taken"
        )
        // Made by a provider where no hub takes Lightning.
        XCTAssertEqual(invoiceWords(amount: 500, asks: 500, gets: 495, by: "the provider"), "You get 495 sat, the provider's fee taken")
    }

    func test_lightning_waits_on_the_providers_asked_where_no_hub_takes_it() {
        XCTAssertTrue(
            closedWays(
                lightning: false,
                silent: true,
                donations: true,
                lookingForDonations: false,
                lookingForLightning: true
            ).elementsEqual([(Licash.Way.lightning, "Looking for a provider…")], by: ==)
        )
        // Found: open, through it.
        XCTAssertTrue(closedWays(lightning: true, silent: true, donations: true, lookingForDonations: false, lookingForLightning: true).isEmpty)
    }

    func test_a_memo_is_its_words_one_space_apart_and_none_if_blank() {
        XCTAssertNil(memoOf(typed: ""))
        XCTAssertNil(memoOf(typed: "  \n\t "))
        XCTAssertEqual(memoOf(typed: "  Lunch \n on\tFriday "), "Lunch on Friday")
        XCTAssertEqual(memoOf(typed: String(repeating: "x", count: MAX_MEMO + 5))?.count, MAX_MEMO)
    }

    func test_a_memo_is_offered_only_where_a_hub_says_one_in_its_invoices() {
        let terms = LightningTerms(pay: Rate(base: 0, ppm: 0), receive: Rate(base: 0, ppm: 0), receiveRange: Range(min: 1, max: 1_000))
        func hub(lightning: LightningTerms?) -> Hub {
            Hub(
                url: "https://hub.example", feeBase: 0, feePpm: 0, feeOutputMsat: 0, deposits: nil,
                silent: false, mailbox: false, lightningPay: nil, lockFee: Rate(base: 0, ppm: 0),
                refundFee: Rate(base: 0, ppm: 0), depositFee: nil, lightning: lightning, contracts: nil
            )
        }
        XCTAssertFalse(memoTaken(hubs: []))
        XCTAssertFalse(memoTaken(hubs: [hub(lightning: nil), hub(lightning: terms)]))
        XCTAssertTrue(memoTaken(hubs: [hub(lightning: terms), hub(lightning: { var c = terms; c.memo = true; return c }())]))
    }

    func test_a_way_not_open_says_why() {
        XCTAssertTrue(
            closedWays(lightning: false, silent: false, donations: false, lookingForDonations: true).elementsEqual(
                [
                    (Licash.Way.lightning, "None of your hubs takes Lightning"),
                    (Licash.Way.silent, "Your hubs don't index silent payments"),
                    (Licash.Way.donations, "Looking for a provider…"),
                ],
                by: ==
            )
        )
        XCTAssertTrue(
            closedWays(lightning: true, silent: true, donations: false, lookingForDonations: false)
                .elementsEqual([(Licash.Way.donations, "No provider takes donations now")], by: ==)
        )
        XCTAssertTrue(closedWays(lightning: true, silent: true, donations: true, lookingForDonations: false).isEmpty)
    }

    func test_a_donation_address_is_another_way_once_a_provider_takes_donations() {
        XCTAssertEqual(ways(lightning: false, silent: false, donations: true), [.code, .bitcoin, .donations])
        XCTAssertEqual(ways(lightning: true, silent: true, donations: true).last, .donations)
        // Never picked for an amount: asked for.
        XCTAssertEqual(picked(amount: 500, lightning: true), .lightning)
    }

    func test_a_donation_address_says_who_pays_it_on_and_for_what() {
        let address = DonationAddress(
            lnurl: "LNURL1DP68GURN8GHJ7",
            provider: "https://swaps.example/",
            name: nil,
            feeBase: 2,
            feePpm: 5_000,
            min: 100,
            max: 1_000_000,
            stocked: true,
            reachable: true
        )
        XCTAssertEqual(donationWords(address: address), "Paid to you in notes by swaps.example, less its fee of 2 sat + 0.5%.")
        XCTAssertEqual(
            donationWords(address: { var c = address; c.name = "Swaps"; c.feeBase = 0; c.feePpm = 0; c.stocked = false; return c }()),
            "Paid to you in notes by Swaps, with no fee. " +
                "It can't take donations right now: payers are refused until it can."
        )
        // Unheard: shown as kept, the phone perhaps offline.
        XCTAssertEqual(
            donationWords(address: { var c = address; c.stocked = false; c.reachable = false; return c }()),
            "Paid to you in notes by swaps.example, less its fee of 2 sat + 0.5%. " +
                "It isn't answering right now: payers may be refused until it does."
        )
    }
}
