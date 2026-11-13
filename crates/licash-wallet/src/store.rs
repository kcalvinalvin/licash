//! The wallet's file: one SQLite database, written in IMMEDIATE
//! transactions, so that processes sharing it see each step whole.

use std::{
    path::Path,
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

use crate::Error;

/// The schema this wallet reads and writes, as SQLite's `user_version`.
const VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE meta (
    one INTEGER PRIMARY KEY CHECK (one = 1),
    seed BLOB NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;
-- Hubs the owner trusts, by the URL it added, and what each last said of
-- itself (an Info body).
CREATE TABLE hubs (
    url TEXT PRIMARY KEY,
    info TEXT NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
-- A keyset belongs to the one hub that first advertised it.
CREATE TABLE keysets (
    id BLOB PRIMARY KEY,
    hub TEXT NOT NULL REFERENCES hubs (url),
    bytes BLOB NOT NULL
) STRICT;
-- Payments made, each step recorded before the hub is asked for the next:
-- the offer asked for, then the release once signed. A signed send is
-- never given up, as its hub may have released it.
CREATE TABLE sends (
    id BLOB PRIMARY KEY,
    hub TEXT NOT NULL REFERENCES hubs (url),
    payee BLOB NOT NULL,
    amount INTEGER NOT NULL,
    fee INTEGER NOT NULL,
    offer TEXT NOT NULL,
    release TEXT,
    unlock BLOB,
    created_at INTEGER NOT NULL,
    done_at INTEGER,
    abandoned TEXT
) STRICT;
-- The owner's notes. Each was made at `position` of `branch` of the
-- proposal with ephemeral key `ephemeral`, where the scan key derives it
-- again.
CREATE TABLE notes (
    y BLOB PRIMARY KEY,
    keyset BLOB NOT NULL REFERENCES keysets (id),
    denom INTEGER NOT NULL,
    note BLOB NOT NULL,
    proposal BLOB NOT NULL,
    ephemeral BLOB NOT NULL,
    branch INTEGER NOT NULL,
    position INTEGER NOT NULL,
    -- The send that spends it, from when that send chose it.
    spent_by BLOB REFERENCES sends (id),
    -- 1 unlinked, 0 linked, -1 linked for good: its hub unlinks it no more.
    unlinked INTEGER NOT NULL DEFAULT 0,
    -- An unlink under way: its session, to ask the hub again.
    session TEXT,
    added_at INTEGER NOT NULL
) STRICT;
CREATE INDEX notes_spent_by ON notes (spent_by);
-- Payments taken.
CREATE TABLE receipts (
    id BLOB PRIMARY KEY,
    keyset BLOB NOT NULL REFERENCES keysets (id),
    amount INTEGER NOT NULL,
    received_at INTEGER NOT NULL
) STRICT;
";

pub(crate) struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// A new wallet file at `path` holding `seed`, readable by its owner
    /// only. Never over an existing file.
    pub(crate) fn create(path: &Path, seed: &[u8; 32]) -> Result<Self, Error> {
        let mut file = std::fs::OpenOptions::new();
        file.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
        file.open(path).map_err(|e| setup(path, e))?;
        let store = Self::connect(path)?;
        store.write(|tx| {
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "user_version", VERSION)?;
            tx.execute(
                "INSERT INTO meta (one, seed, created_at) VALUES (1, ?1, ?2)",
                rusqlite::params![seed, now()],
            )?;
            Ok(())
        })?;
        Ok(store)
    }

    /// The wallet file at `path`, and its seed.
    pub(crate) fn open(path: &Path) -> Result<(Self, [u8; 32]), Error> {
        if !path.exists() {
            return Err(Error::Setup(format!("no wallet at {}", path.display())));
        }
        let store = Self::connect(path)?;
        let seed = store.read(|c| {
            let version: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
            if version != VERSION {
                return Err(Error::Setup(format!(
                    "wallet schema {version}, this version reads {VERSION}"
                )));
            }
            Ok(c.query_row("SELECT seed FROM meta", [], |r| r.get(0))?)
        })?;
        Ok((store, seed))
    }

    fn connect(path: &Path) -> Result<Self, Error> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags).map_err(|e| setup(path, e))?;
        conn.busy_timeout(Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn read<T>(
        &self,
        op: impl FnOnce(&Connection) -> Result<T, Error>,
    ) -> Result<T, Error> {
        op(&self.conn())
    }

    /// `op` whole or not at all.
    pub(crate) fn write<T>(
        &self,
        op: impl FnOnce(&Transaction<'_>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = op(&tx)?;
        tx.commit()?;
        Ok(value)
    }
}

fn setup(path: &Path, e: impl std::fmt::Display) -> Error {
    Error::Setup(format!("wallet {}: {e}", path.display()))
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A u64 as SQLite's i64, which every amount and denomination fits.
pub(crate) fn sql(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
