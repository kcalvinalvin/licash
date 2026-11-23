import XCTest
@testable import Licash

class PaymentNoteTest: XCTestCase {
    func test_a_note_is_its_words_one_space_apart_and_none_if_blank() {
        XCTAssertNil(noteOf(typed: ""))
        XCTAssertNil(noteOf(typed: " \n\t "))
        XCTAssertEqual(noteOf(typed: "  for the \n pizza "), "for the pizza")
    }

    func test_a_note_typed_is_cut_to_whole_characters_within_its_bytes() {
        XCTAssertEqual(
            fitNote(typed: String(repeating: "x", count: MAX_NOTE_BYTES + 3)),
            String(repeating: "x", count: MAX_NOTE_BYTES)
        )
        // Two bytes each: a whole number of them.
        XCTAssertEqual(
            fitNote(typed: String(repeating: "\u{E9}", count: MAX_NOTE_BYTES)),
            String(repeating: "\u{E9}", count: MAX_NOTE_BYTES / 2)
        )
        // Four bytes each, one code point: never split.
        let emoji = "\u{1F355}"
        let fitted = fitNote(typed: "a" + String(repeating: emoji, count: 60))
        XCTAssertEqual(fitted.utf8.count, 1 + 4 * 49)
        XCTAssertEqual(fitNote(typed: "short"), "short")
    }
}
