import LicashCore
import SwiftUI

/*
 * Contacts: the people the wallet pays, by name, each a payment code
 * kept. Picked to pay instead of scanning; kept after paying a code.
 */

/// The contacts to pay, by name: a tap pays one; each may be forgotten. Nothing while there are none.
struct Contacts: View {
    let engine: Engine
    let pay: (Contact) -> Void

    @SwiftUI.State private var contacts: [Contact] = []
    @SwiftUI.State private var forgetting: Contact?

    init(engine: Engine, pay: @escaping (Contact) -> Void) {
        self.engine = engine
        self.pay = pay
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if !contacts.isEmpty {
                Text("Contacts")
                    .textStyle(Theme.type.labelLarge)
                    .foregroundStyle(Theme.color.onSurfaceVariant)
                    .padding(.top, 24)
                    .padding(.bottom, 4)
                ForEach(contacts, id: \.self) { c in
                    HStack(alignment: .center, spacing: 0) {
                        Button { pay(c) } label: {
                            Text(c.name)
                                .textStyle(Theme.type.bodyLarge)
                                .padding(.leading, 4)
                                .frame(maxWidth: .infinity, minHeight: 48, alignment: .leading)
                                .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                        .accessibilityIdentifier("contact \(c.name)")
                        IconButton(onClick: { forgetting = c }) {
                            Image(systemName: "xmark").accessibilityLabel("Forget \(c.name)")
                        }
                    }
                    .frame(maxWidth: .infinity)
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .task { if case .success(let it) = await engine.contacts() { contacts = it } }
        .sheet(isPresented: Binding(get: { forgetting != nil }, set: { if !$0 { forgetting = nil } })) {
            if let c = forgetting {
                Sheet(title: "Forget \(c.name)?", dismiss: { forgetting = nil }) {
                    Say(text: "Their code is forgotten: to pay them again, scan it again.")
                    BigButton(text: "Forget", danger: true) {
                        forgetting = nil
                        Task {
                            _ = await engine.removeContact(name: c.name)
                            if case .success(let it) = await engine.contacts() { contacts = it }
                        }
                    }
                    .frame(maxWidth: .infinity)
                    .accessibilityIdentifier("forget-contact")
                }
            }
        }
    }
}

/// After paying the code `code`, or being paid by its wallet if `payer`:
/// whom, if a contact, else keeping it as one, by a name.
struct SaveContact: View {
    let engine: Engine
    let code: String
    let payer: Bool

    @SwiftUI.State private var looked = false
    @SwiftUI.State private var known: Contact?
    @SwiftUI.State private var naming = false
    @SwiftUI.State private var name = ""
    @SwiftUI.State private var error: String?

    init(engine: Engine, code: String, payer: Bool = false) {
        self.engine = engine
        self.code = code
        self.payer = payer
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if looked {
                VStack(alignment: .leading, spacing: 0) {
                    if let contact = known {
                        Text(payer ? "From \(contact.name)" : "To \(contact.name)")
                            .textStyle(Theme.type.bodyMedium)
                            .foregroundStyle(Theme.color.onSurfaceVariant)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .accessibilityIdentifier("contact")
                    } else if naming {
                        OutlinedTextField(value: name, onValueChange: { name = $0 }, label: "Their name", singleLine: true)
                            .textInputAutocapitalization(.words)
                            .submitLabel(.done)
                            .frame(maxWidth: .infinity)
                            .accessibilityIdentifier("contact-name")
                        if let error {
                            Text(error)
                                .textStyle(Theme.type.bodyLarge)
                                .foregroundStyle(Theme.color.error)
                        }
                        Spacer().frame(height: 12)
                        BigButton(text: "Save", enabled: !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty) {
                            Task {
                                switch await engine.saveContact(name: name, code: code) {
                                case .success(let it):
                                    known = it
                                    naming = false
                                case .failure(let e):
                                    error = explain(e: e)
                                }
                            }
                        }
                        .frame(maxWidth: .infinity)
                        .accessibilityIdentifier("save-contact-name")
                    } else {
                        BigButton(text: "Save as a contact", quiet: true) {
                            naming = true
                        }
                        .frame(maxWidth: .infinity)
                        .accessibilityIdentifier("save-contact")
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.top, 12)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .task(id: code) {
            known = try? await engine.contactOf(code: code).get()
            looked = true
        }
    }
}
