//! The provider's file: its swaps and its events, in one SQLite database
//! beside its wallet's, written in IMMEDIATE transactions.

use std::{
    path::Path,
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

use crate::Error;

/// The schema this provider reads and writes, as SQLite's `user_version`.
const VERSION: i64 = 3;

/// Version 3: swaps into the provider's own hub. A swap of kind `own` has
/// hub `to_url`, which the provider is, lock notes it issues itself
/// (`licash_hub::Hub::lock_own`) rather than notes its wallet holds there.
const ISSUES: &str = "
-- The own swaps' locks, by their swaps' ids: their notes pay the client's
-- address as derived from the ephemeral secret, recorded with the swap,
-- before the hub locks.
CREATE TABLE issues (
    id BLOB PRIMARY KEY REFERENCES swaps (id),
    ephemeral BLOB NOT NULL
) STRICT;
";

/// Version 2: payouts on Bitcoin. A swap's kind is `notes`, a swap of notes
/// at one hub for notes at another, or `chain`, a payout: its `to_url` is
/// `bitcoin:<network>`, `address` the output script of the provider's coin,
/// `amount` its value and `expiry` its timeout, a block height.
const PAYOUTS: &str = "
ALTER TABLE swaps ADD COLUMN kind TEXT NOT NULL DEFAULT 'notes';
-- The payouts' coins, by their swaps' ids: claimed with the client's key
-- claim_key and the preimage, taken back after the timeout with the
-- provider's refund_key, a secret key, recorded before the coin is made.
-- funding_tx is the wallet's transaction paying it, at output
-- funding_vout, once made; refund_tx the provider's taking it back once
-- timed out unclaimed. scanned is the last block looked in for its spend.
CREATE TABLE payouts (
    id BLOB PRIMARY KEY REFERENCES swaps (id),
    claim_key BLOB NOT NULL,
    refund_key BLOB NOT NULL,
    funding_tx BLOB,
    funding_vout INTEGER,
    refund_tx BLOB,
    scanned INTEGER
) STRICT;
";

/// Version 1, as providers in the field hold it
/// (`tests/fixtures/swaps-v1.sql`): never change it, add a migration.
const SCHEMA: &str = "
CREATE TABLE meta (
    one INTEGER PRIMARY KEY CHECK (one = 1),
    -- The secret key the provider announces itself under on Nostr.
    nostr BLOB NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;
-- Swaps by the client's lock at `from_url`, which pays the provider `paid`
-- under `hash` until `expiry_from`: recorded before the provider's lock of
-- `amount` to the client's `address` at `to_url` is made, then that lock,
-- then the preimage the client revealed claiming it. `state` is accepted,
-- locked, paid, done, refunded, failed or lost (paid at `to_url` but not
-- claimed at `from_url`).
CREATE TABLE swaps (
    id BLOB PRIMARY KEY,
    hash BLOB NOT NULL UNIQUE,
    from_url TEXT NOT NULL,
    to_url TEXT NOT NULL,
    address BLOB NOT NULL,
    amount INTEGER NOT NULL,
    fee INTEGER NOT NULL,
    paid INTEGER NOT NULL,
    expiry_from INTEGER NOT NULL,
    expiry INTEGER NOT NULL,
    lock BLOB,
    preimage BLOB,
    state TEXT NOT NULL,
    reason TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
CREATE INDEX swaps_state ON swaps (state);
-- What the operator is told of, in order.
CREATE TABLE events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    json TEXT NOT NULL
) STRICT;
";

pub(crate) struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// A new file at `path`, readable by its owner only. Never over an
    /// existing file.
    pub(crate) fn create(path: &Path, nostr: &[u8; 32]) -> Result<Self, Error> {
        let mut file = std::fs::OpenOptions::new();
        file.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
        file.open(path).map_err(|e| setup(path, e))?;
        let store = Self::connect(path)?;
        store.write(|tx| {
            tx.execute_batch(SCHEMA)?;
            tx.execute_batch(PAYOUTS)?;
            tx.execute_batch(ISSUES)?;
            tx.pragma_update(None, "user_version", VERSION)?;
            tx.execute(
                "INSERT INTO meta (one, nostr, created_at) VALUES (1, ?1, ?2)",
                rusqlite::params![nostr, now()],
            )?;
            Ok(())
        })?;
        Ok(store)
    }

    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        if !path.exists() {
            return Err(Error::Setup(format!("no swaps file at {}", path.display())));
        }
        let store = Self::connect(path)?;
        // Checked and migrated in one transaction: a provider of another
        // version opening it meanwhile sees it whole, either way.
        store.write(|tx| {
            let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
            match version {
                VERSION => {}
                1 | 2 => {
                    if version == 1 {
                        tx.execute_batch(PAYOUTS)?;
                    }
                    tx.execute_batch(ISSUES)?;
                    tx.pragma_update(None, "user_version", VERSION)?;
                    tracing::info!("swaps file migrated to schema {VERSION}");
                }
                _ => {
                    return Err(Error::Setup(format!(
                        "swaps schema {version}, this version reads 1 to {VERSION}"
                    )))
                }
            }
            Ok(())
        })?;
        Ok(store)
    }

    fn connect(path: &Path) -> Result<Self, Error> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags).map_err(|e| setup(path, e))?;
        conn.busy_timeout(Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
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
    Error::Setup(format!("swaps {}: {e}", path.display()))
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
