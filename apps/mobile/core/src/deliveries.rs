//! Payments to codes naming a mailbox, kept from before they are made until
//! sent there: one its hub did not answer is made later, by
//! [`Wallet::resume`](crate::Wallet::resume), and sent then; one whose
//! mailbox was not reached is sent again.
//!
//! Kept in `deliveries.json` in the wallet's directory, written whole: no
//! file, none kept.

use std::{
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use licash::address::Address;
use serde::{Deserialize, Serialize};

/// A payment to send to its payee's mailbox once made.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Delivery {
    /// The payee's Licash address and the amount, as the payment records
    /// them.
    pub payee: Address,
    pub amount: u64,
    /// The payee's mailbox key.
    pub mailbox: [u8; 32],
    /// When it was kept, in seconds since 1970: its payment is no older.
    pub at: i64,
    /// The payment's id, once known: none if the app stopped while paying.
    #[serde(default)]
    pub send: Option<String>,
}

fn file(dir: &Path) -> PathBuf {
    dir.join("deliveries.json")
}

/// Seconds since 1970.
pub(crate) fn now() -> i64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    since.map_or(0, |d| d.as_secs() as i64)
}

/// The deliveries kept in `dir`.
pub(crate) fn load(dir: &Path) -> Vec<Delivery> {
    let Ok(kept) = std::fs::read(file(dir)) else {
        return Vec::new();
    };
    serde_json::from_slice(&kept).unwrap_or_else(|e| {
        tracing::warn!(%e, "the payments to send to mailboxes unreadable");
        Vec::new()
    })
}

/// Keep `deliveries` in `dir` in place of those kept: written aside, then
/// moved over them.
pub(crate) fn save(dir: &Path, deliveries: &[Delivery]) -> io::Result<()> {
    let path = file(dir);
    let aside = path.with_extension("json.new");
    let mut written = File::create(&aside)?;
    written.write_all(&serde_json::to_vec(deliveries)?)?;
    written.sync_all()?;
    std::fs::rename(aside, path)
}

/// Remove one delivery like `delivery` from `all`.
pub(crate) fn forget(all: &mut Vec<Delivery>, delivery: &Delivery) {
    if let Some(at) = all.iter().position(|d| d == delivery) {
        all.remove(at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deliveries_are_kept_whole_and_none_without_a_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_empty());
        let payee = licash::address::OwnerKeys::from_seed(&[1; 32])
            .unwrap()
            .address();
        let one = Delivery {
            payee,
            amount: 5,
            mailbox: [7; 32],
            at: now(),
            send: None,
        };
        let mut all = vec![one.clone(), one.clone()];
        save(dir.path(), &all).unwrap();
        assert_eq!(load(dir.path()), all);
        forget(&mut all, &one);
        save(dir.path(), &all).unwrap();
        assert_eq!(load(dir.path()), [one]);
        std::fs::write(file(dir.path()), "{").unwrap();
        assert!(load(dir.path()).is_empty());
    }
}
