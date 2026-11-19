//! Explicit secret export and a consistent SQLite snapshot, including WAL.

use std::path::Path;

use crate::{Error, Transport, Wallet};

impl<T: Transport> Wallet<T> {
    /// The original seed, only for an explicitly requested private backup.
    /// A wallet created without a stored seed cannot export one.
    pub fn backup_seed(&self) -> Result<[u8; 32], Error> {
        self.store.read(|c| {
            c.query_row("SELECT seed FROM meta", [], |row| {
                row.get::<_, Option<[u8; 32]>>(0)
            })?
            .ok_or_else(|| Error::Setup("this wallet does not store its seed".into()))
        })
    }

    /// Snapshot all committed state of the wallet's file under the
    /// connection lock: the wallet's own, pending payments and note
    /// reservations included, and the tables other components keep in the
    /// same file (contracts, a swap provider's), so that one copy is the
    /// whole of it. The destination must not exist. This never checkpoints
    /// or copies a live main file without its WAL.
    pub fn backup_to(&self, to: &Path) -> Result<(), Error> {
        self.store.read(|c| {
            licash_store::backup(c, to).map_err(|e| match e {
                licash_store::Error::Sqlite(e) => Error::Storage(e),
                e => Error::Setup(format!("a private backup: {e}")),
            })
        })
    }
}
