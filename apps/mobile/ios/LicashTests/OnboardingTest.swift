import LicashCore
import XCTest
@testable import Licash

class OnboardingTest: XCTestCase {
    func test_the_networks_show_once_asked_for_and_never_hide_one_chosen() {
        XCTAssertFalse(networksShown(asked: false, ui: Ui()))
        XCTAssertTrue(networksShown(asked: true, ui: Ui()))
        // A test network chosen stays in sight, so it is never made unseen.
        XCTAssertTrue(networksShown(asked: false, ui: Ui(network: .mutinynet)))
        XCTAssertTrue(networksShown(asked: false, ui: Ui(developer: true)))
    }

    func test_a_wallet_that_cannot_be_opened_says_why_and_how_to_start_over() {
        let said = unopenedWords(reason: "A wallet of an earlier version, left as it is.")
        XCTAssertTrue(said.hasPrefix("A wallet of an earlier version, left as it is. To start over"))
        XCTAssertTrue(said.contains("delete this app"))
        XCTAssertTrue(said.contains("lost"))
    }

    func test_networks_are_named_as_proper_nouns() {
        XCTAssertEqual(
            [Network.bitcoin, .signet, .mutinynet, .regtest].map { named(network: $0) },
            ["Bitcoin", "Signet", "Mutinynet", "Regtest"]
        )
    }

    func test_no_hub_on_bitcoin_says_how_to_try_it_and_nothing_elsewhere() {
        XCTAssertEqual(noneFound(network: .bitcoin, none: true), NONE_ON_BITCOIN)
        // A wallet with a hub already, or on a test network: only "None found".
        XCTAssertNil(noneFound(network: .bitcoin, none: false))
        XCTAssertNil(noneFound(network: .mutinynet, none: true))
        XCTAssertNil(noneFound(network: .signet, none: true))
    }
}
