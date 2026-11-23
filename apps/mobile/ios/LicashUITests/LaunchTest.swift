import XCTest

final class LaunchTest: XCTestCase {
    func testLaunches() {
        let app = XCUIApplication()
        app.launch()
        XCTAssertTrue(app.staticTexts["placeholder"].waitForExistence(timeout: 30))
    }
}
