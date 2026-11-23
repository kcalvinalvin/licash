import LicashCore
import XCTest
@testable import Licash

class FlowTest: XCTestCase {
    private func note(amount: UInt64) -> NoteLine {
        NoteLine(
            id: "n\(amount)", hub: "https://hub.example", amount: amount, madeBy: "p", madeByItself: false,
            spentBy: nil, linking: .unlinked, gone: false, addedAt: 0
        )
    }

    private func part(to: PaidTo, amount: UInt64, notes: [UInt64]) -> NotesPaid {
        NotesPaid(to: to, hub: "https://hub.example", amount: amount, notes: notes)
    }

    func test_a_note_twice_as_large_is_a_bit_longer() {
        XCTAssertEqual(bits(amount: 0), 0)
        XCTAssertEqual(bits(amount: 1), 1)
        XCTAssertEqual(bits(amount: 4_096), 13)
        XCTAssertEqual(bits(amount: 8_191), 13)
        XCTAssertEqual(bits(amount: 8_192), 14)
        XCTAssertEqual(bits(amount: UInt64.max), 64)
    }

    func test_a_payment_is_drawn_payee_first_its_empty_parts_left_out() throws {
        let flow = try XCTUnwrap(
            flowOf(
                notes: NotesOf(
                    spent: [note(amount: 4_096), note(amount: 2_048)],
                    made: [note(amount: 1_024)],
                    paid: [
                        part(to: .kept, amount: 1_024, notes: [1_024]),
                        part(to: .hub, amount: 0, notes: []),
                        part(to: .payee, amount: 5_000, notes: [4_096, 512, 256, 128, 8]),
                    ]
                )
            )
        )
        XCTAssertEqual(flow.spent, [4_096, 2_048])
        XCTAssertEqual(flow.out.map(\.to), [.payee, .kept])
        XCTAssertEqual(flow.out[0].amount, 5_000)
    }

    func test_received_none_spent_the_notes_made_are_the_wallets() throws {
        let flow = try XCTUnwrap(flowOf(notes: NotesOf(spent: [], made: [note(amount: 512), note(amount: 8)], paid: [])))
        XCTAssertEqual(flow.spent, [])
        XCTAssertEqual(flow.out, [Bucket(to: nil, amount: 520, notes: [512, 8])])
    }

    func test_nothing_spent_or_made_draws_nothing() {
        XCTAssertNil(flowOf(notes: NotesOf(spent: [], made: [], paid: [])))
    }

    func test_parts_are_headed_in_a_word() {
        XCTAssertEqual(shortTo(kind: .payout, to: .payee), "Provider")
        XCTAssertEqual(shortTo(kind: .lightning, to: .payee), "Invoice")
        XCTAssertEqual(shortTo(kind: .sent, to: .payee), "Payee")
        XCTAssertEqual(shortTo(kind: .sent, to: .hub), "Hub fee")
        XCTAssertEqual(shortTo(kind: .received, to: nil), "Yours")
    }
}
