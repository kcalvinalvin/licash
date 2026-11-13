//! The hub's ledger: one SQLite connection, one IMMEDIATE transaction per
//! operation, and a write counter that halts the hub if its database is
//! replaced while it runs.

use std::{
    fs::{File, OpenOptions, TryLockError},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, MutexGuard, PoisonError,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

use crate::Error;

/// The schema this hub reads and writes, as SQLite's `user_version`.
pub(crate) const VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE meta (
    one INTEGER PRIMARY KEY CHECK (one = 1),
    -- Bumped by every write, and checked against the running hub's count.
    seq INTEGER NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;
CREATE TABLE keysets (
    id BLOB PRIMARY KEY,
    idx INTEGER NOT NULL UNIQUE,
    max_pow INTEGER NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;
-- Every proposal the hub served: a grant or a lock.
CREATE TABLE proposals (
    id BLOB PRIMARY KEY,
    keyset BLOB NOT NULL REFERENCES keysets (id),
    created_at INTEGER NOT NULL,
    -- When a spend of its inputs by another proposal was first refused.
    double_spent_at INTEGER
) STRICT;
-- Offered grants. The unlock derives from the seed and the salt, the
-- unlink nonces from the boot that made the offer.
CREATE TABLE grants (
    id BLOB PRIMARY KEY REFERENCES proposals (id),
    offer BLOB NOT NULL,
    salt BLOB NOT NULL,
    boot BLOB NOT NULL,
    signed BLOB,
    released_at INTEGER
) STRICT;
-- Checkpoints the terms of a lock may name.
CREATE TABLE checkpoints (
    commitment BLOB PRIMARY KEY,
    height INTEGER NOT NULL,
    bytes BLOB NOT NULL,
    added_at INTEGER NOT NULL
) STRICT;
-- The best chain as scanned, each block linked to the one before, from
-- the first checkpoint's.
CREATE TABLE blocks (
    height INTEGER PRIMARY KEY,
    hash BLOB NOT NULL,
    prev BLOB NOT NULL
) STRICT;
-- Every activation ever scanned, in the best chain or not: a holder may
-- prove one on any chain from a checkpoint. Never deleted.
CREATE TABLE activations (
    block BLOB NOT NULL,
    txid BLOB NOT NULL,
    output INTEGER NOT NULL,
    hash BLOB NOT NULL,
    secret BLOB NOT NULL,
    height INTEGER NOT NULL,
    PRIMARY KEY (block, txid, output)
) STRICT;
CREATE INDEX activations_hash ON activations (hash);
-- Timed locks: their pay branch is issued on the preimage, their refund
-- branch once they expired unactivated. `own` locks spend notes the hub
-- issued itself, backed by what the caller holds (an HTLC).
CREATE TABLE locks (
    id BLOB PRIMARY KEY REFERENCES proposals (id),
    hash BLOB NOT NULL,
    checkpoint BLOB NOT NULL REFERENCES checkpoints (commitment),
    expiry INTEGER NOT NULL,
    depth INTEGER NOT NULL,
    -- Height by which the hub answers a refund request: expiry, depth and
    -- its reaction.
    decide_by INTEGER NOT NULL,
    proposal BLOB NOT NULL,
    signed BLOB NOT NULL,
    certificate BLOB NOT NULL,
    amount INTEGER NOT NULL,
    own INTEGER NOT NULL,
    preimage BLOB,
    settled TEXT CHECK (settled IN ('pay', 'refund')),
    settled_at INTEGER
) STRICT;
CREATE INDEX locks_hash ON locks (hash);
CREATE UNIQUE INDEX own_locks ON locks (hash) WHERE own = 1;
-- Each retired input, by its identifier, with the note that spent it.
CREATE TABLE spent (
    y BLOB PRIMARY KEY,
    proposal BLOB NOT NULL REFERENCES proposals (id),
    position INTEGER NOT NULL,
    note BLOB NOT NULL
) STRICT;
-- Each output the hub issued or, without a boot, reserved for a lock's
-- branch; a lock's with its issuance, and once answered, its unlink
-- record. A reserved output is issued to nobody else.
CREATE TABLE issued (
    keyset BLOB NOT NULL REFERENCES keysets (id),
    denom INTEGER NOT NULL,
    blinded BLOB NOT NULL,
    commitment BLOB NOT NULL,
    proposal BLOB NOT NULL REFERENCES proposals (id),
    position INTEGER NOT NULL,
    boot BLOB,
    issuance BLOB CHECK (issuance IS NULL OR boot IS NOT NULL),
    answered BLOB CHECK (answered IS NULL OR boot IS NOT NULL),
    PRIMARY KEY (keyset, denom, blinded)
) STRICT;
-- Notes paid to the hub, a lock's refund branch from `REFUND_INDEX`.
CREATE TABLE income (
    proposal BLOB NOT NULL REFERENCES proposals (id),
    position INTEGER NOT NULL,
    denom INTEGER NOT NULL,
    note BLOB NOT NULL,
    PRIMARY KEY (proposal, position)
) STRICT;
-- The notification outbox.
CREATE TABLE events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    json TEXT NOT NULL
) STRICT;
";

pub(crate) struct Store {
    inner: Mutex<Inner>,
    halted: AtomicBool,
    /// Held for the life of the hub: one hub per database.
    _lock: File,
}

struct Inner {
    conn: Connection,
    seq: i64,
}

impl Store {
    /// A new ledger at `path`, which must not exist, filled by `init` in
    /// the transaction that creates the schema. On an error nothing is
    /// left behind.
    pub(crate) fn create(
        path: &Path,
        init: impl FnOnce(&Transaction<'_>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| setup_io(path, e))?;
        let created = (|| {
            let mut conn = connect(path)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "user_version", VERSION)?;
            tx.execute(
                "INSERT INTO meta (one, seq, created_at) VALUES (1, 0, ?1)",
                [now()],
            )?;
            init(&tx)?;
            tx.commit()?;
            Ok(())
        })();
        if created.is_err() {
            for file in [
                path.to_owned(),
                suffixed(path, "-wal"),
                suffixed(path, "-shm"),
            ] {
                let _ = std::fs::remove_file(file);
            }
        }
        created
    }

    /// The ledger at `path`, as [`Store::create`] made it.
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        if !path.is_file() {
            return Err(Error::Setup(format!(
                "no hub database at {}",
                path.display()
            )));
        }
        // One lock per file, by whatever path the hub is given.
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(suffixed(&path.canonicalize()?, ".lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(Error::Setup(format!(
                    "another hub is running on {}",
                    path.display()
                )))
            }
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
        let conn = connect(path)?;
        check_version(&conn)?;
        let seq = conn.query_row("SELECT seq FROM meta", [], |r| r.get(0))?;
        Ok(Self {
            inner: Mutex::new(Inner { conn, seq }),
            halted: AtomicBool::new(false),
            _lock: lock,
        })
    }

    /// Run `op` in one IMMEDIATE transaction and commit it if `op`
    /// succeeds. An [`Error::Halted`] from `op`, or a database whose write
    /// count is not this hub's, halts the hub.
    pub(crate) fn write<T>(
        &self,
        op: impl FnOnce(&Transaction<'_>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let mut inner = self.lock()?;
        let Inner { conn, seq } = &mut *inner;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute("UPDATE meta SET seq = seq + 1 WHERE seq = ?1", [*seq])? != 1 {
            return Err(self.halt("the database changed under the running hub"));
        }
        let value = match op(&tx) {
            Err(Error::Halted) => return Err(self.halt("the ledger contradicts itself")),
            result => result?,
        };
        tx.commit()?;
        *seq += 1;
        Ok(value)
    }

    pub(crate) fn read<T>(
        &self,
        op: impl FnOnce(&Connection) -> Result<T, Error>,
    ) -> Result<T, Error> {
        op(&self.lock()?.conn)
    }

    pub(crate) fn halted(&self) -> bool {
        self.halted.load(Ordering::SeqCst)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>, Error> {
        if self.halted.load(Ordering::SeqCst) {
            return Err(Error::Halted);
        }
        // A panic in an operation rolled its transaction back.
        Ok(self.inner.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn halt(&self, why: &str) -> Error {
        self.halted.store(true, Ordering::SeqCst);
        tracing::error!("{why}: the hub serves nothing more");
        Error::Halted
    }
}

/// A connection to the ledger at `path` that cannot write it.
pub(crate) fn inspect(path: &Path) -> Result<Connection, Error> {
    if !path.is_file() {
        return Err(Error::Setup(format!(
            "no hub database at {}",
            path.display()
        )));
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(10))?;
    conn.pragma_update(None, "query_only", true)?;
    check_version(&conn)?;
    Ok(conn)
}

fn check_version(conn: &Connection) -> Result<(), Error> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version != VERSION {
        return Err(Error::Setup(format!(
            "database schema {version}, this hub reads {VERSION}"
        )));
    }
    Ok(())
}

fn connect(path: &Path) -> Result<Connection, Error> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(10))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    let journal: String =
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    let synchronous: i64 = conn.pragma_query_value(None, "synchronous", |r| r.get(0))?;
    if !journal.eq_ignore_ascii_case("wal") || synchronous != 2 {
        return Err(Error::Setup(format!(
            "database durability: journal {journal}, synchronous {synchronous}"
        )));
    }
    Ok(conn)
}

pub(crate) fn setup_io(path: &Path, e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::AlreadyExists {
        Error::Setup(format!("{} exists", path.display()))
    } else {
        e.into()
    }
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
