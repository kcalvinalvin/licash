import Foundation

/// A folder of a tree the user chose (picked in Files, reached in its
/// security scope), as far as backups need one: its files listed,
/// written whole, read, deleted. Each call reaches the folder's provider:
/// off the main thread.
final class Folder {
    /// A file or folder in it.
    struct Child: Equatable {
        var name: String
        var bytes: Int64
        var id: String
        var dir: Bool
    }

    /// The folder the user chose, its scope opened around each call.
    private let tree: URL
    private let id: String

    private init(tree: URL, id: String) {
        self.tree = tree
        self.id = id
    }

    private var uri: URL { URL(fileURLWithPath: id) }

    /// Its own name.
    func name() throws -> String? { try accessing { try query(of: uri).name } }

    func children() throws -> [Child] {
        try accessing {
            try coordinated(at: uri) { at in
                try FileManager.default.contentsOfDirectory(at: at, includingPropertiesForKeys: [.nameKey, .fileSizeKey, .isDirectoryKey])
                    .map { try query(of: $0) }
            }
        }
    }

    /// The folder `name` in it, if there.
    func find(name: String) throws -> Folder? {
        try children().first { $0.dir && $0.name == name }.map { Folder(tree: tree, id: $0.id) }
    }

    /// The folder `name` in it, made if not there.
    func dir(name: String) throws -> Folder {
        if let found = try find(name: name) { return found }
        let made = uri.appendingPathComponent(name)
        try accessing { try FileManager.default.createDirectory(at: made, withIntermediateDirectories: false) }
        return Folder(tree: tree, id: made.path)
    }

    /// `from` copied in as `name`, any file of that name replaced, and read
    /// back as long as it: a file it could not write whole is an error.
    func write(name: String, from: URL) throws {
        try accessing {
            try delete(name: name)
            try coordinated(at: uri.appendingPathComponent(name), writing: .forReplacing) { made in
                try FileManager.default.copyItem(at: from, to: made)
                let written = (try? query(of: made))?.bytes
                let length = ((try? FileManager.default.attributesOfItem(atPath: from.path))?[.size] as? NSNumber)?.int64Value ?? 0
                if let written, written >= 0, written != length {
                    throw IOException(message: "\(name) written in part: \(written) of \(length) bytes")
                }
            }
        }
    }

    /// The file `child` of it copied `to`, if it holds at most `most` bytes:
    /// else nothing of it kept, and false. Its size as the provider tells it
    /// is not trusted, only what it gives.
    func read(child: Child, to: URL, most: Int64) throws -> Bool {
        if child.bytes > most { return false }
        return try accessing {
            try coordinated(at: URL(fileURLWithPath: child.id)) { at in
                guard let input = InputStream(url: at) else {
                    throw IOException(message: "\(child.name) cannot be read")
                }
                return try copyAtMost(input: input, to: to, most: most)
            }
        }
    }

    /// Every file `name` in it deleted.
    func delete(name: String) throws {
        try accessing {
            for c in try children() where !c.dir && c.name == name {
                try coordinated(at: URL(fileURLWithPath: c.id), writing: .forDeleting) { try FileManager.default.removeItem(at: $0) }
            }
        }
    }

    private func query(of: URL) throws -> Child {
        let values = try of.resourceValues(forKeys: [.nameKey, .fileSizeKey, .isDirectoryKey])
        return Child(
            name: values.name ?? "",
            bytes: values.fileSize.map(Int64.init) ?? -1,
            id: of.path,
            dir: values.isDirectory == true
        )
    }

    /// `call` on the item `at`, at the URL its provider hands it: a read
    /// once the item is downloaded, a write or a delete told to the provider.
    private func coordinated<T>(at: URL, writing: NSFileCoordinator.WritingOptions? = nil, call: (URL) throws -> T) throws -> T {
        var failed: NSError?
        var done: Result<T, Error>?
        if let writing {
            NSFileCoordinator().coordinate(writingItemAt: at, options: writing, error: &failed) { url in done = Result { try call(url) } }
        } else {
            NSFileCoordinator().coordinate(readingItemAt: at, options: [], error: &failed) { url in done = Result { try call(url) } }
        }
        if let failed { throw failed }
        return try done!.get()
    }

    /// `call` with the folder the user chose in reach.
    private func accessing<T>(call: () throws -> T) rethrows -> T {
        let open = tree.startAccessingSecurityScopedResource()
        defer { if open { tree.stopAccessingSecurityScopedResource() } }
        return try call()
    }

    /// The folder the user chose, `tree`.
    static func of(tree: URL) -> Folder { Folder(tree: tree, id: tree.path) }
}

/// Bytes read at a time in a copy.
let DEFAULT_BUFFER_SIZE = 8 * 1024

/// `input` copied `to`, if it holds at most `most` bytes: else, or failing,
/// nothing of it kept, and false. Never more than `most` bytes written,
/// whatever the stream holds. `input` is opened here, and closed once read.
func copyAtMost(input: InputStream, to: URL, most: Int64) throws -> Bool {
    var buffer = [UInt8](repeating: 0, count: DEFAULT_BUFFER_SIZE)
    var copied: Int64 = 0
    var whole = false
    defer { if !whole { try? FileManager.default.removeItem(at: to) } }
    input.open()
    defer { input.close() }
    try Data().write(to: to)
    let out = try FileHandle(forWritingTo: to)
    defer { try? out.close() }
    while true {
        let n = input.read(&buffer, maxLength: buffer.count)
        if n < 0 { throw input.streamError ?? CocoaError(.fileReadUnknown) }
        if n == 0 {
            whole = true
            break
        }
        copied += Int64(n)
        if copied > most { break }
        try out.write(contentsOf: buffer[0..<n])
    }
    return whole
}

/// A file the folder could not make, write or read as asked.
private struct IOException: LocalizedError {
    var message: String
    var errorDescription: String? { message }
}
