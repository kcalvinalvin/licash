import XCTest

final class LaunchTest: XCTestCase {
    func testLaunches() {
        let app = XCUIApplication()
        app.launch()
        // A new install opens on making a wallet.
        XCTAssertTrue(app.descendants(matching: .any)["create"].waitForExistence(timeout: 60))
    }
}
