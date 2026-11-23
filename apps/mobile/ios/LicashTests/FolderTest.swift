import XCTest
@testable import Licash

final class FolderTest: XCTestCase {
    private func scratch() throws -> URL {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("folder-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: dir) }
        return dir
    }

    func test_aStreamOfAtMostTheLimitIsCopiedWhole() throws {
        let to = try scratch().appendingPathComponent("a.lcb")
        let bytes = Data((0..<20_000).map { UInt8(truncatingIfNeeded: $0) })
        XCTAssertTrue(try copyAtMost(input: InputStream(data: bytes), to: to, most: Int64(bytes.count)))
        XCTAssertEqual(try Data(contentsOf: to), bytes)
        XCTAssertTrue(try copyAtMost(input: InputStream(data: Data()), to: to, most: 0))
        XCTAssertTrue(try FileManager.default.fileExists(atPath: to.path) && Data(contentsOf: to).isEmpty)
    }

    func test_aStreamPastTheLimitLeavesNothing() throws {
        let to = try scratch().appendingPathComponent("a.lcb")
        let bytes = Data(count: 20_001)
        XCTAssertFalse(try copyAtMost(input: InputStream(data: bytes), to: to, most: 20_000))
        XCTAssertFalse(FileManager.default.fileExists(atPath: to.path))
    }

    func test_anEndlessStreamIsReadNoFurtherThanPastTheLimit() throws {
        let to = try scratch().appendingPathComponent("a.lcb")
        let endless = Endless(data: Data())
        XCTAssertFalse(try copyAtMost(input: endless, to: to, most: 1_000_000))
        XCTAssertFalse(FileManager.default.fileExists(atPath: to.path))
        XCTAssertTrue(endless.bytesRead <= 1_000_000 + DEFAULT_BUFFER_SIZE, "read \(endless.bytesRead)")
    }

    func test_aStreamFailingPartwayLeavesNothing() throws {
        let to = try scratch().appendingPathComponent("a.lcb")
        XCTAssertThrowsError(try copyAtMost(input: Failing(data: Data()), to: to, most: 1_000_000), "a failing stream copied")
        XCTAssertFalse(FileManager.default.fileExists(atPath: to.path))
    }
}

/// A stream giving as many bytes as asked, for ever.
private final class Endless: InputStream {
    var bytesRead = 0

    override func open() {}
    override func close() {}
    override var hasBytesAvailable: Bool { true }

    override func read(_ buffer: UnsafeMutablePointer<UInt8>, maxLength len: Int) -> Int {
        bytesRead += len
        return len
    }
}

/// A stream giving as many bytes as asked once, then failing.
private final class Failing: InputStream {
    var given = 0

    override func open() {}
    override func close() {}
    override var hasBytesAvailable: Bool { true }
    override var streamError: Error? { given > 1 ? NSError(domain: "FolderTest", code: 0, userInfo: [NSLocalizedDescriptionKey: "gone"]) : nil }

    override func read(_ buffer: UnsafeMutablePointer<UInt8>, maxLength len: Int) -> Int {
        given += 1
        return given > 1 ? -1 : len
    }
}
