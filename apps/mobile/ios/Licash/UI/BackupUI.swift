import CoreTransferable
import LicashCore
import SwiftUI
import UIKit
import UniformTypeIdentifiers

private let ONE_COPY = "Use it on one phone only: a wallet live on two phones can lose money."

/// Settings' backup page: the recovery words, automatic backups, and a file of everything.
struct BackupPage: View {
    let engine: Engine
    let ui: Ui
    @SwiftUI.State private var words = false
    @SwiftUI.State private var auto = false
    @SwiftUI.State private var file = false

    var body: some View {
        Group(title: "") {
            Entry(title: "Recovery words", sub: "Your keys, on paper", tag: "backup-words") { words = true }
            Divider()
            Entry(title: "Automatic backup", sub: autoBackupState(backup: ui.backup), tag: "backup-auto") { auto = true }
            Divider()
            Entry(title: "Backup file", sub: "Everything, as of now", tag: "backup-file") { file = true }
        }
        .sheet(isPresented: $words) { Phrase(engine: engine) { words = false } }
        .sheet(isPresented: $auto) { AutoBackupSheet(engine: engine, ui: ui) { auto = false } }
        .sheet(isPresented: $file) { BackupFile(engine: engine, ui: ui) { file = false } }
    }
}

/// How automatic backups stand, in a line.
func autoBackupState(backup: AutoBackup?) -> String {
    guard let backup else { return "Off: everything, to a folder you sync" }
    if let problem = backup.problem { return "Failing: \(problem)" }
    guard let written = backup.at else { return "On, none written yet" }
    return "On, last \(at(seconds: written / 1000))"
}

/// Automatic backups: a folder chosen, written to whenever the wallet changes.
private struct AutoBackupSheet: View {
    let engine: Engine
    let ui: Ui
    let dismiss: () -> Void
    @SwiftUI.State private var busy = false
    @SwiftUI.State private var error: String? = nil
    @SwiftUI.State private var choosing = false

    private func run(step: @escaping () async -> Result<Void, Error>) {
        busy = true
        error = nil
        Task {
            if case .failure(let e) = await step() { error = explain(e: e) }
            busy = false
        }
    }

    var body: some View {
        let backup = ui.backup
        Sheet(title: "Automatic backup", dismiss: dismiss, dismissable: !busy) {
            Say(
                text: "Everything this wallet holds, encrypted with your recovery words, saved to a folder each time it " +
                    "changes. Choose one a sync app (Syncthing, Nextcloud) copies off this phone."
            )
            Say(text: "Restoring needs the folder and your 24 words.")
            if let backup {
                Line(label: "Last backup", value: backup.at.map { at(seconds: $0 / 1000) } ?? "None yet", tag: "backup-at")
                if let problem = backup.problem { Problem(text: problem) }
            }
            if let error { Problem(text: error) }
            Spacer().frame(height: 8)
            if backup == nil {
                BigButton(text: busy ? "Saving…" : "Choose folder", enabled: !busy) { choosing = true }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .accessibilityIdentifier("backup-choose")
            } else {
                BigButton(text: busy ? "Saving…" : "Back up now", enabled: !busy) {
                    run { await engine.backupAgain() }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .accessibilityIdentifier("backup-now")
                Spacer().frame(height: 8)
                BigButton(text: "Another folder", enabled: !busy, quiet: true) { choosing = true }
                    .frame(maxWidth: .infinity, alignment: .leading)
                Spacer().frame(height: 8)
                BigButton(text: "Turn off", enabled: !busy, quiet: true) {
                    engine.stopBackups()
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .accessibilityIdentifier("backup-off")
            }
        }
        .fileImporter(isPresented: $choosing, allowedContentTypes: [.folder]) { result in
            if case .success(let uri) = result { run { await engine.startBackups(tree: uri) } }
        }
    }
}

/// The 24 words, behind a warning and a tap, hidden from screen recordings and the app switcher while shown.
private struct Phrase: View {
    let engine: Engine
    let dismiss: () -> Void
    @SwiftUI.State private var words: [String]? = nil
    @SwiftUI.State private var birthday: UInt32? = nil
    @SwiftUI.State private var error: String? = nil
    @SwiftUI.State private var copied = false

    var body: some View {
        Sheet(title: "Recovery words", dismiss: dismiss, secure: true) {
            switch words {
            case nil:
                Say(text: "Anyone with these words can take your money. Write them on paper and never share them.")
                Say(text: "They bring back your keys and coins on Bitcoin. Money in hubs needs the backup file.")
                if let error { Problem(text: error) }
                BigButton(text: "Show words") {
                    error = nil
                    Task {
                        switch await engine.backupPhrase() {
                        case .success(let w): words = w
                        case .failure(let e): error = explain(e: e)
                        }
                        birthday = try? await engine.birthday().get()
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .accessibilityIdentifier("show-words")
            case let shown?:
                WordList(words: shown)
                Spacer().frame(height: 12)
                if let birthday {
                    Line(label: "Wallet birthday", value: "Block \(digits(amount: UInt64(birthday)))", tag: "birthday")
                    Say(text: "Write it down with the words: restoring looks for your coins from there, much faster.")
                }
                Spacer().frame(height: 4)
                if copied { Say(text: "Paste them where you keep them safe, then copy something else over them.") }
                BigButton(text: "Done", onClick: dismiss)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .accessibilityIdentifier("words-done")
                Spacer().frame(height: 8)
                BigButton(text: copied ? "Copied" : "Copy words", quiet: true) {
                    secret(text: shown.joined(separator: " "))
                    copied = true
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .accessibilityIdentifier("copy-words")
            }
        }
    }
}

/// `text` copied, marked sensitive: iOS keeps it on this phone, off the user's other devices.
private func secret(text: String) {
    UIPasteboard.general.setItems([[UTType.utf8PlainText.identifier: text]], options: [.localOnly: true])
}

/// The words numbered, in two columns read down the first then the second.
private struct WordList: View {
    let words: [String]

    var body: some View {
        let half = (words.count + 1) / 2
        HStack(alignment: .top, spacing: 24) {
            ForEach([0..<half, half..<words.count], id: \.self) { column in
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(column, id: \.self) { i in
                        HStack(alignment: .top, spacing: 0) {
                            Text("\(i + 1)")
                                .textStyle(Theme.type.bodyLarge)
                                .foregroundStyle(Theme.color.outline)
                                .frame(width: 28, alignment: .leading)
                            Text(words[i])
                                .textStyle(Theme.type.bodyLarge)
                                .accessibilityIdentifier("word \(i + 1)")
                        }
                        .padding(.vertical, 4)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("words")
    }
}

/// A file of everything the wallet holds, saved where the user picks.
private struct BackupFile: View {
    let engine: Engine
    let ui: Ui
    let dismiss: () -> Void
    @SwiftUI.State private var busy = false
    @SwiftUI.State private var saved = false
    @SwiftUI.State private var error: String? = nil
    // Written whole first, then saved where the user picks: iOS asks where
    // only once it has the file.
    @SwiftUI.State private var made: URL? = nil

    var body: some View {
        Sheet(title: "Backup file", dismiss: dismiss, dismissable: !busy) {
            if saved {
                Say(text: "Saved. Save a new one after new payments.")
                BigButton(text: "Done", onClick: dismiss)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .accessibilityIdentifier("backup-done")
            } else {
                Say(text: "Everything this wallet holds, as of now. It is not encrypted: keep it private.")
                Say(text: ONE_COPY)
                if let error { Problem(text: error) }
                BigButton(text: busy ? "Saving…" : "Save file", enabled: !busy) {
                    busy = true
                    error = nil
                    let file = FileManager.default.temporaryDirectory.appendingPathComponent("backup-\(UUID().uuidString).licash")
                    Task {
                        switch await engine.exportBackup(to: file) {
                        case .success: made = file
                        case .failure(let e): error = explain(e: e)
                        }
                        busy = false
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .accessibilityIdentifier("save-backup")
            }
        }
        // A file left by a run closed while asking where to save it: removed, as it is not encrypted.
        .onAppear {
            let tmp = FileManager.default.temporaryDirectory
            for f in (try? FileManager.default.contentsOfDirectory(at: tmp, includingPropertiesForKeys: nil)) ?? []
            where f.lastPathComponent.hasPrefix("backup-") && f.pathExtension == "licash" {
                try? FileManager.default.removeItem(at: f)
            }
        }
        .fileExporter(
            isPresented: Binding(get: { made != nil }, set: { if !$0, let file = made { try? FileManager.default.removeItem(at: file); made = nil } }),
            item: made.map { BackupMade(file: $0) },
            contentTypes: [.data],
            defaultFilename: backupName(network: ui.network, on: Date())
        ) { result in
            switch result {
            case .success: saved = true
            case .failure(let e): error = explain(e: e)
            }
        }
    }
}

/// A file written whole, to save where the user picks.
private struct BackupMade: Transferable {
    let file: URL

    static var transferRepresentation: some TransferRepresentation {
        FileRepresentation(exportedContentType: .data) { SentTransferredFile($0.file) }
    }
}

/// No wallet yet: one brought back from a backup file, or from its 24 words.
struct Restore: View {
    let engine: Engine
    let dismiss: () -> Void
    @SwiftUI.State private var typing = false
    @SwiftUI.State private var text = ""
    @SwiftUI.State private var born = ""
    @SwiftUI.State private var busy = false
    @SwiftUI.State private var error: String? = nil
    // From a backup folder: the folder, then the words, then the backup found.
    @SwiftUI.State private var folder: URL? = nil
    @SwiftUI.State private var found: BackupFound? = nil
    // The one picker, open: for a backup folder if `choosing`, else for a backup file.
    @SwiftUI.State private var picking = false
    @SwiftUI.State private var choosing = false

    private func run(restore: @escaping () async -> Result<Void, Error>) {
        busy = true
        error = nil
        Task {
            if case .failure(let e) = await restore() { error = explain(e: e) }
            busy = false
        }
    }

    var body: some View {
        Sheet(title: "Restore a wallet", dismiss: dismiss, dismissable: !busy, secure: typing) {
            if !typing {
                Say(text: ONE_COPY)
                if let error { Problem(text: error) }
                Entry(title: "Backup folder", sub: "Everything, as last backed up", tag: "restore-folder") {
                    if !busy {
                        choosing = true
                        picking = true
                    }
                }
                Divider()
                Entry(title: "Backup file", sub: busy ? "Restoring…" : "Everything, as saved", tag: "restore-file") {
                    if !busy {
                        choosing = false
                        picking = true
                    }
                }
                Divider()
                Entry(title: "Recovery words", sub: "Your keys only", tag: "restore-words") { if !busy { typing = true } }
            } else {
                let words = phraseWords(text: text)
                if let chosen = folder, let shown = found {
                    Say(text: "Your backup of \(at(seconds: Int64(truncatingIfNeeded: shown.madeAt))): everything as it was then.")
                    if shown.newerUnusable > 0 {
                        Problem(
                            text: "\(shown.newerUnusable) newer backups there are incomplete, perhaps still syncing. " +
                                "Wait for the sync to finish, or restore this older one."
                        )
                    }
                    if shown.writers > 1 {
                        Problem(
                            text: "Backups of \(shown.writers) phones are in this folder: this is the newest, " +
                                "not necessarily the one holding the most."
                        )
                    }
                    Say(text: ONE_COPY)
                    if let error { Problem(text: error) }
                    BigButton(text: busy ? "Restoring…" : "Restore", enabled: !busy) {
                        run { await engine.restoreFolder(tree: chosen, words: words, generation: shown.generation) }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .accessibilityIdentifier("restore")
                } else {
                    Say(
                        text: folder != nil
                            ? "Your 24 words open the backups in the folder."
                            : "Brings back your keys and coins on Bitcoin, not money in hubs. Finding coins can take hours."
                    )
                    OutlinedTextField(
                        value: text,
                        onValueChange: { text = $0; error = nil },
                        label: "24 words",
                        isError: words.count > PHRASE_WORDS,
                        supportingText: "\(words.count) of \(PHRASE_WORDS)",
                        minLines: 5
                    )
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .keyboardType(.asciiCapable)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .accessibilityIdentifier("phrase")
                    if let chosen = folder {
                        if let error { Problem(text: error) }
                        Spacer().frame(height: 12)
                        BigButton(text: busy ? "Looking…" : "Find backup", enabled: !busy && words.count == PHRASE_WORDS) {
                            run {
                                await engine.findFolderBackup(tree: chosen, words: words).flatMap { backup in
                                    guard let backup else {
                                        return .failure(NSError(
                                            domain: "org.licash.wallet",
                                            code: 0,
                                            userInfo: [NSLocalizedDescriptionKey: "No backup of these words there, on this network."]
                                        ))
                                    }
                                    found = backup
                                    return .success(())
                                }
                            }
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .accessibilityIdentifier("find-backup")
                    } else {
                        Spacer().frame(height: 8)
                        let typed = born.trimmingCharacters(in: .whitespacesAndNewlines)
                        let birthday = typed.isEmpty ? nil : UInt32(typed: typed)
                        let badBirthday = !typed.isEmpty && (birthday == nil || birthday == 0)
                        OutlinedTextField(
                            value: born,
                            onValueChange: { v in
                                born = String(v.unicodeScalars.filter { $0.properties.numericType == .decimal })
                                error = nil
                            },
                            label: "Wallet birthday (optional)",
                            isError: badBirthday,
                            supportingText: badBirthday ? "A block number" : "The block written with the words. Empty: looks through all of Bitcoin.",
                            singleLine: true
                        )
                        .keyboardType(.numberPad)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .accessibilityIdentifier("restore-birthday")
                        if let error { Problem(text: error) }
                        Spacer().frame(height: 12)
                        BigButton(text: busy ? "Restoring…" : "Restore", enabled: !busy && words.count == PHRASE_WORDS && !badBirthday) {
                            run { await engine.restorePhrase(words: words, birthday: birthday) }
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .accessibilityIdentifier("restore")
                    }
                }
            }
        }
        .fileImporter(isPresented: $picking, allowedContentTypes: choosing ? [.folder] : [.item]) { result in
            guard case .success(let uri) = result else { return }
            if choosing {
                folder = uri
                typing = true
            } else {
                run { await engine.restoreBackup(from: uri) }
            }
        }
    }
}

/// Advanced: look through Bitcoin again, after a yes.
struct ResetChain: View {
    let engine: Engine
    @SwiftUI.State private var asking = false
    @SwiftUI.State private var busy = false
    @SwiftUI.State private var error: String? = nil

    var body: some View {
        Entry(title: "Reset chain state", tag: "reset-chain") { error = nil; asking = true }
            .sheet(isPresented: $asking) {
                Sheet(title: "Reset chain state?", dismiss: { asking = false }, dismissable: !busy) {
                    Say(text: "Downloads Bitcoin's blocks again and looks for your payments from the start. Nothing you hold is lost; it can take a while.")
                    if let error { Problem(text: error) }
                    BigButton(text: busy ? "Resetting…" : "Reset", enabled: !busy) {
                        busy = true
                        Task {
                            switch await engine.resetChain() {
                            case .success: asking = false
                            case .failure(let e): error = explain(e: e)
                            }
                            busy = false
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .accessibilityIdentifier("reset")
                    Spacer().frame(height: 12)
                    BigButton(text: "Cancel", enabled: !busy, quiet: true) { asking = false }
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
    }
}
