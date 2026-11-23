import LicashCore
import XCTest

final class CoreTest: XCTestCase {
    func testBlockSeconds() {
        XCTAssertEqual(blockSeconds(network: .bitcoin), 600)
    }
}
