import XCTest
@testable import Licash

class WaitedTest: XCTestCase {
    func test_saysNothingBeforeAnyWait() {
        let wait = Wait()
        wait.turn(waiting: false, now: 1_000)
        XCTAssertNil(wait.says(now: 5_000))
    }

    func test_ticksWholeSecondsWhileWaiting() {
        let wait = Wait()
        wait.turn(waiting: true, now: 1_000)
        XCTAssertEqual(wait.says(now: 1_999), "0 s")
        XCTAssertEqual(wait.says(now: 13_500), "12 s")
        XCTAssertEqual(wait.says(now: 66_000), "1 min 05 s")
    }

    func test_saysHowLongItTookOnceOver() {
        let wait = Wait()
        wait.turn(waiting: true, now: 1_000)
        // Told again while still waiting: the start stays.
        wait.turn(waiting: true, now: 2_000)
        wait.turn(waiting: false, now: 4_250)
        XCTAssertEqual(wait.says(now: 9_000), "Took 3.2 s")
        wait.turn(waiting: false, now: 9_000)
        XCTAssertEqual(wait.says(now: 10_000), "Took 3.2 s")
    }

    func test_aNewWaitForgetsTheLast() {
        let wait = Wait()
        wait.turn(waiting: true, now: 0)
        wait.turn(waiting: false, now: 2_000)
        wait.turn(waiting: true, now: 5_000)
        XCTAssertEqual(wait.says(now: 6_000), "1 s")
    }

    func test_anUnnoticeableWaitGoesUnsaid() {
        let wait = Wait()
        wait.turn(waiting: true, now: 0)
        wait.turn(waiting: false, now: 99)
        XCTAssertNil(wait.says(now: 200))
    }

    func test_lastedIsPreciseUnderTenSecondsThenClockLike() {
        XCTAssertEqual(lasted(ms: 450), "0.4 s")
        XCTAssertEqual(lasted(ms: 9_999), "9.9 s")
        XCTAssertEqual(lasted(ms: 12_300), "12 s")
        XCTAssertEqual(lasted(ms: 125_000), "2 min 05 s")
    }
}
