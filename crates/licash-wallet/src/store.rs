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
    -- The owner's seed, unless the application keeps it elsewhere.
    seed BLOB,
    -- The owner's address, which the seed opening the wallet must derive.
    address BLOB NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;
-- Hubs the owner trusts, and what each last said of itself (an Info body).
CREATE TABLE hubs (
    id INTEGER PRIMARY KEY,
    info TEXT NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
-- The URLs each hub is reached at: one each, for now.
CREATE TABLE hub_urls (
    url TEXT PRIMARY KEY,
    hub INTEGER NOT NULL REFERENCES hubs (id)
) STRICT;
CREATE INDEX hub_urls_hub ON hub_urls (hub);
-- Each hub with the URL the wallet reaches it at.
CREATE VIEW reach (hub, url) AS
    SELECT hub, url FROM hub_urls
    WHERE rowid IN (SELECT MIN(rowid) FROM hub_urls GROUP BY hub);
-- A keyset belongs to the one hub that first advertised it.
CREATE TABLE keysets (
    id BLOB PRIMARY KEY,
    hub INTEGER NOT NULL REFERENCES hubs (id),
    bytes BLOB NOT NULL
) STRICT;
-- Payments made, each step recorded before the hub is asked for the next:
-- the offer asked for, then the release once signed. A signed send is
-- given up only once its hub refused it as spent, which it never releases.
CREATE TABLE sends (
    id BLOB PRIMARY KEY,
    hub INTEGER NOT NULL REFERENCES hubs (id),
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
    -- The send or lock that spends it, from when it chose it.
    spent_by BLOB,
    -- 1 unlinked, 0 linked, -1 linked for good: its hub unlinks it no more.
    unlinked INTEGER NOT NULL DEFAULT 0,
    -- An unlink under way: its session, to ask the hub again.
    session TEXT,
    -- When its hub showed it spent by a payment this file never made, by a
    -- copy of the wallet: it is never spent here.
    gone_at INTEGER,
    added_at INTEGER NOT NULL
) STRICT;
CREATE INDEX notes_spent_by ON notes (spent_by);
-- Notes locked to a payee on the preimage of `hash` until `expiry`, else
-- refunded: the lock asked for (a Lock body), then the hub's certificate,
-- then the branch its hub settled, once the wallet kept its notes of it.
-- An uncertified lock is given up only once its hub refused it, which
-- proves the hub never recorded it. A lock to the hub may pay a Lightning
-- `invoice` through it, and record why the hub failed or refused to.
CREATE TABLE locks (
    id BLOB PRIMARY KEY,
    hub INTEGER NOT NULL REFERENCES hubs (id),
    payee BLOB NOT NULL,
    hash BLOB NOT NULL,
    expiry INTEGER NOT NULL,
    amount INTEGER NOT NULL,
    fee INTEGER NOT NULL,
    request TEXT NOT NULL,
    certificate BLOB,
    -- 'pay' or 'refund'.
    settled TEXT,
    preimage BLOB,
    invoice TEXT,
    failure TEXT,
    created_at INTEGER NOT NULL,
    done_at INTEGER,
    abandoned TEXT
) STRICT;
-- Payments asked for over Lightning, by their hash: the preimage only the
-- wallet knows and the receipt asked for (a Receive body), recorded before
-- the hub hears of them, then the hub's invoice and, once it was paid, the
-- lock of the hub's notes the wallet claims. Done once claimed; canceled
-- once the hub refused it or canceled the invoice.
CREATE TABLE invoices (
    hash BLOB PRIMARY KEY,
    hub INTEGER NOT NULL REFERENCES hubs (id),
    preimage BLOB NOT NULL,
    amount INTEGER NOT NULL,
    body TEXT NOT NULL,
    request TEXT,
    lock BLOB,
    created_at INTEGER NOT NULL,
    done_at INTEGER,
    canceled TEXT
) STRICT;
-- Swaps of notes at one hub for notes at another through a provider, by
-- the hash of a preimage only the wallet knows, recorded before the lock to
-- the provider (in locks, under that hash) is made; then the provider's
-- lock to the wallet at the other hub, which the wallet claims with the
-- preimage. Done once claimed; failed once the provider did not lock.
CREATE TABLE swaps (
    hash BLOB PRIMARY KEY,
    provider TEXT NOT NULL,
    preimage BLOB NOT NULL,
    from_hub INTEGER NOT NULL REFERENCES hubs (id),
    to_hub INTEGER NOT NULL REFERENCES hubs (id),
    amount INTEGER NOT NULL,
    fee INTEGER NOT NULL,
    theirs BLOB,
    created_at INTEGER NOT NULL,
    done_at INTEGER,
    failed TEXT
) STRICT;
-- Payments taken, and locks claimed.
CREATE TABLE receipts (
    id BLOB PRIMARY KEY,
    keyset BLOB NOT NULL REFERENCES keysets (id),
    amount INTEGER NOT NULL,
    received_at INTEGER NOT NULL
) STRICT;
";

/// What a wallet file holds of its owner.
pub(crate) struct Owner {
    /// Unless the application keeps it.
    pub(crate) seed: Option<[u8; 32]>,
    pub(crate) address: Vec<u8>,
}

pub(crate) struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// A new wallet file at `path` of the owner at `address`, holding its
    /// `seed` if given, readable by its owner only. Never over an existing
    /// file.
    pub(crate) fn create(
        path: &Path,
        seed: Option<&[u8; 32]>,
        address: &[u8],
    ) -> Result<Self, Error> {
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
                "INSERT INTO meta (one, seed, address, created_at) VALUES (1, ?1, ?2, ?3)",
                rusqlite::params![seed, address, now()],
            )?;
            Ok(())
        })?;
        Ok(store)
    }

    /// The wallet file at `path`, and what it holds of its owner.
    pub(crate) fn open(path: &Path) -> Result<(Self, Owner), Error> {
        if !path.exists() {
            return Err(Error::Setup(format!("no wallet at {}", path.display())));
        }
        let store = Self::connect(path)?;
        let owner = store.read(|c| {
            let version: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
            if version != VERSION {
                return Err(Error::Setup(format!(
                    "wallet schema {version}, this version reads {VERSION}"
                )));
            }
            Ok(c.query_row("SELECT seed, address FROM meta", [], |r| {
                Ok(Owner {
                    seed: r.get(0)?,
                    address: r.get(1)?,
                })
            })?)
        })?;
        Ok((store, owner))
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
