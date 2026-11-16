//! Explicit secret export and a consistent SQLite snapshot, including WAL.

use std::{fs::OpenOptions, path::Path};

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

    /// Snapshot all committed wallet state under the connection lock. This
    /// includes pending payments and note reservations, not just the seed.
    /// The destination must not exist. This never checkpoints or copies a
    /// live main file without its WAL.
    pub fn backup_to(&self, to: &Path) -> Result<(), Error> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options
            .open(to)
            .map_err(|e| Error::Setup(format!("creating a private backup: {e}")))?;
        let result = self.store.read(|c| {
            c.backup(rusqlite::MAIN_DB, to, None)?;
            Ok(())
        });
        if result.is_err() {
            drop(file);
            let _ = std::fs::remove_file(to);
        } else {
            file.sync_all()
                .map_err(|e| Error::Setup(format!("syncing a private backup: {e}")))?;
        }
        result
    }
}
