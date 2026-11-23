import XCTest
@testable import Licash

class FeelTest: XCTestCase {
    func test_an_outcome_is_confirmed_or_rejected_from_android_11_and_a_press_before() {
        // One for every iOS the app runs on, 17 and later.
        XCTAssertEqual(haptic(touch: .done), .success)
        XCTAssertEqual(haptic(touch: .failed), .error)
    }

    func test_a_switch_has_its_own_tick_from_android_14() {
        XCTAssertEqual(haptic(touch: .on), .selection)
        XCTAssertEqual(haptic(touch: .off), .selection)
    }

    func test_done_and_failed_never_feel_alike() {
        XCTAssertNotEqual(haptic(touch: .done), haptic(touch: .failed))
    }
}
