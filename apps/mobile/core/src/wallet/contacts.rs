//! The people the wallet pays, by name: an address book of payment codes,
//! one to each Licash wallet (its address on its network), whatever else
//! its code says. Kept in `contacts.json` in the wallet's directory, gone
//! with it, written whole, aside first: no file, none kept. One that cannot
//! be read is never written over.

use std::{
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
};

use licash::spend::ChainProfile;
use licash_wallet::PaymentCode;
use serde::{Deserialize, Serialize};

use super::Wallet;
use crate::LicashError;

/// The most characters a name may have.
const MAX_NAME: usize = 64;

/// The most contacts kept.
const MAX_CONTACTS: usize = 1_000;

/// Held while the contacts change: read, changed and written whole.
static CHANGING: Mutex<()> = Mutex::new(());

/// Someone the wallet pays, by name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct Contact {
    pub name: String,
    /// Their payment code, asking no amount.
    pub code: String,
}

fn file(dir: &Path) -> PathBuf {
    dir.join("contacts.json")
}

/// The contacts kept in `dir`: refused if unreadable, never none instead.
fn load(dir: &Path) -> Result<Vec<Contact>, LicashError> {
    let unreadable = |e: &dyn std::fmt::Display| {
        LicashError::failed(format!("the contacts kept are unreadable: {e}"))
    };
    match std::fs::read(file(dir)) {
        Ok(kept) => serde_json::from_slice(&kept).map_err(|e| unreadable(&e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(unreadable(&e)),
    }
}

/// Keep `contacts` in `dir` in place of those kept: written aside, then
/// moved over them.
fn save(dir: &Path, contacts: &[Contact]) -> Result<(), LicashError> {
    let path = file(dir);
    let aside = path.with_extension("json.new");
    let written = (|| -> io::Result<()> {
        let mut written = File::create(&aside)?;
        written.write_all(&serde_json::to_vec(contacts)?)?;
        written.sync_all()?;
        std::fs::rename(&aside, &path)
    })();
    written.map_err(|e| LicashError::failed(format!("keeping the contacts: {e}")))
}

/// `text` as a contact keeps it: a code paying in notes on `network`,
/// asking no amount. Its fallback address left out where a silent payment
/// address takes its place: paid again, it would be reused.
fn kept_code(text: &str, network: ChainProfile) -> Result<PaymentCode, LicashError> {
    let mut code: PaymentCode = text.trim().parse().map_err(LicashError::invalid)?;
    code.check(network)?;
    if code.licash.is_none() {
        return Err(LicashError::invalid(
            "not a Licash code: a contact is paid in notes",
        ));
    }
    code.amount = None;
    if code.silent.is_some() {
        code.address = None;
    }
    Ok(code)
}

/// Whether the code `text` pays the same Licash wallet as `code`.
fn pays_same(text: &str, code: &PaymentCode) -> bool {
    let Ok(other) = text.parse::<PaymentCode>() else {
        return false;
    };
    match (&other.licash, &code.licash) {
        (Some(a), Some(b)) => a.network == b.network && a.address == b.address,
        _ => false,
    }
}

/// `name`, trimmed: refused if empty or too long.
fn named(name: &str) -> Result<String, LicashError> {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() || name.chars().count() > MAX_NAME {
        return Err(LicashError::invalid(format!(
            "a name of 1 to {MAX_NAME} characters"
        )));
    }
    Ok(name)
}

/// `all`, `code` kept as `name`'s in place of the contact paying the same
/// wallet: renamed, or its code made new. Refused for another's name.
fn keep(
    mut all: Vec<Contact>,
    name: String,
    code: &PaymentCode,
) -> Result<(Vec<Contact>, Contact), LicashError> {
    all.retain(|c| !pays_same(&c.code, code));
    if all
        .iter()
        .any(|c| c.name.to_lowercase() == name.to_lowercase())
    {
        return Err(LicashError::invalid(format!(
            "{name} is another contact's name already"
        )));
    }
    if all.len() >= MAX_CONTACTS {
        return Err(LicashError::invalid(format!(
            "{MAX_CONTACTS} contacts at most"
        )));
    }
    let contact = Contact {
        name,
        code: code.to_string(),
    };
    all.push(contact.clone());
    Ok((all, contact))
}

fn changing() -> std::sync::MutexGuard<'static, ()> {
    CHANGING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Contacts: the people the wallet pays, by name.
#[uniffi::export]
impl Wallet {
    /// The contacts, by name.
    pub fn contacts(&self) -> Result<Vec<Contact>, LicashError> {
        let mut all = load(&self.dir)?;
        all.sort_by_key(|c| c.name.to_lowercase());
        Ok(all)
    }

    /// Keep the payment code `code` as `name`'s, asking no amount: in
    /// place of the contact paying the same wallet, renamed or its code
    /// made new. Refused for a name another contact has, or a code paying
    /// on Bitcoin only.
    pub fn save_contact(&self, name: String, code: String) -> Result<Contact, LicashError> {
        let name = named(&name)?;
        let code = kept_code(&code, self.wallet.network())?;
        let _changing = changing();
        let (all, contact) = keep(load(&self.dir)?, name, &code)?;
        save(&self.dir, &all)?;
        Ok(contact)
    }

    /// Forget the contact named `name`, if there is one.
    pub fn remove_contact(&self, name: String) -> Result<(), LicashError> {
        let _changing = changing();
        let mut all = load(&self.dir)?;
        let before = all.len();
        all.retain(|c| c.name != name);
        if all.len() < before {
            save(&self.dir, &all)?;
        }
        Ok(())
    }

    /// The contact the payment code `code` pays, if one does: whom a
    /// payment goes to.
    pub fn contact_of(&self, code: String) -> Result<Option<Contact>, LicashError> {
        let Ok(code) = code.trim().parse::<PaymentCode>() else {
            return Ok(None);
        };
        let all = load(&self.dir)?;
        Ok(all.into_iter().find(|c| pays_same(&c.code, &code)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contacts_are_kept_whole_and_an_unreadable_file_never_written_over() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unwrap(), Vec::new());
        let bob = Contact {
            name: "Bob".into(),
            code: "bitcoin:?licash=x".into(),
        };
        save(dir.path(), std::slice::from_ref(&bob)).unwrap();
        assert_eq!(load(dir.path()).unwrap(), vec![bob]);
        assert!(!file(dir.path()).with_extension("json.new").exists());
        std::fs::write(file(dir.path()), b"[{\"name\": ").unwrap();
        let refused = load(dir.path()).unwrap_err();
        assert!(format!("{refused:?}").contains("unreadable"), "{refused:?}");
    }

    #[test]
    fn names_are_trimmed_and_neither_empty_nor_too_long() {
        assert_eq!(named("  Bob \n the  baker ").unwrap(), "Bob the baker");
        assert!(named("   ").is_err());
        assert!(named(&"é".repeat(MAX_NAME)).is_ok());
        assert!(named(&"é".repeat(MAX_NAME + 1)).is_err());
    }
}
