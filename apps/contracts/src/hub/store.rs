//! The hub's openings, in `openings.sqlite` of its data directory: each
//! as its machine stored itself at its last step, the digest of the
//! holder's message that step took, and the hub's own coins F spends,
//! which its node keeps locked while the opening lives.
//!
//! The file's version tells which of its tables a build must know to
//! read it right. The `fees` are none of them: made with the first kept,
//! whatever the version, and ignored by a build before them, which opens
//! the file all the same. A migration to come must make them
//! `IF NOT EXISTS` too.

use std::path::Path;

use anyhow::{bail, Result};
use bitcoin::consensus::{deserialize, serialize};
use bitcoin::OutPoint;
use licash_api::Fees;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

/// The format of the rows: a machine's stored bytes, of
/// `licash_contract`'s format 1.
pub(super) const FORMAT: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS openings (
    id BLOB PRIMARY KEY,
    format INTEGER NOT NULL,
    phase INTEGER NOT NULL,
    mock INTEGER NOT NULL,
    irrevocable INTEGER NOT NULL,
    deposit BLOB NOT NULL,
    coins BLOB NOT NULL,
    last BLOB NOT NULL,
    state BLOB NOT NULL,
    abandoned INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
CREATE UNIQUE INDEX IF NOT EXISTS openings_deposit ON openings (deposit) WHERE abandoned = 0;
";

/// What the hub charged as each opening started, as JSON: what its
/// holder agreed to pay at most, for the operator to know whom fees
/// raised since leave to agree again. Nothing is done by it. No row, of an
/// opening started before these were kept, tells nothing: nor does no
/// table.
const FEES: &str = "
CREATE TABLE IF NOT EXISTS fees (
    id BLOB PRIMARY KEY REFERENCES openings (id),
    fees TEXT NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;
";

/// An opening as stored.
pub(super) struct Row {
    pub state: Vec<u8>,
    /// The digest of the holder's message its last step took.
    pub last: [u8; 32],
    pub abandoned: bool,
}

/// An opening as stored, for the hub's operator ([`Store::listed`]).
pub(super) struct Listed {
    pub id: [u8; 32],
    pub format: i64,
    pub phase: u8,
    pub mock: bool,
    pub irrevocable: bool,
    pub abandoned: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub state: Vec<u8>,
}

/// The openings, and what the hub charges, if told
/// ([`Store::charging`]): kept with each that starts.
pub(super) struct Store(Connection, Option<Fees>);

impl Store {
    /// How many openings in `dir` the hub signed and did not give up,
    /// read only: none if no opening was ever stored there.
    pub fn signed(dir: &Path) -> Result<u64> {
        let path = dir.join("openings.sqlite");
        if !path.exists() {
            return Ok(0);
        }
        let c = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let n: i64 = c.query_row(
            "SELECT count(*) FROM openings WHERE irrevocable = 1 AND abandoned = 0",
            [],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// Every opening in `dir`, oldest first, read only: none if no
    /// opening was ever stored there.
    pub fn listed(dir: &Path) -> Result<Vec<Listed>> {
        let path = dir.join("openings.sqlite");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let c = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let version: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => return Ok(Vec::new()),
            1 => {}
            v => bail!("openings.sqlite is of version {v}, newer than this hub knows"),
        }
        let mut stmt = c.prepare(
            "SELECT id, format, phase, mock, irrevocable, abandoned, created_at, updated_at, state
             FROM openings ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Listed {
                id: r.get(0)?,
                format: r.get(1)?,
                phase: r.get(2)?,
                mock: r.get(3)?,
                irrevocable: r.get(4)?,
                abandoned: r.get(5)?,
                created_at: r.get(6)?,
                updated_at: r.get(7)?,
                state: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn open(dir: &Path) -> Result<Self> {
        let c = crate::private_db(&dir.join("openings.sqlite"))?;
        let version: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => {
                c.execute_batch(SCHEMA)?;
                c.pragma_update(None, "user_version", 1)?;
            }
            1 => {}
            v => bail!("openings.sqlite is of version {v}, newer than this hub knows"),
        }
        Ok(Self(c, None))
    }

    /// Keep `fees`, what the hub charges, with each opening that starts
    /// from now on.
    pub fn charging(&mut self, fees: Fees) {
        self.1 = Some(fees);
    }

    /// What the hub charged as each opening in `dir` it signed and did
    /// not give up started, read only: none of those started before these
    /// were kept, nor of one whose own cannot be read.
    pub fn charged(dir: &Path) -> Result<Vec<Fees>> {
        let path = dir.join("openings.sqlite");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let c = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let made: Option<i64> = c
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'fees'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if made.is_none() {
            return Ok(Vec::new());
        }
        let mut stmt = c.prepare(
            "SELECT f.fees FROM fees f JOIN openings o ON o.id = f.id
             WHERE o.irrevocable = 1 AND o.abandoned = 0",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut all = Vec::new();
        for text in rows {
            if let Ok(fees) = serde_json::from_str(&text?) {
                all.push(fees);
            }
        }
        Ok(all)
    }

    /// Opening `id` in `dir`, read only, beside the running hub: none if
    /// it is not there, an error if no opening was ever stored there.
    pub fn read(dir: &Path, id: &[u8; 32]) -> Result<Option<Row>> {
        let path = dir.join("openings.sqlite");
        if !path.exists() {
            bail!("no openings in {}", dir.display());
        }
        let c = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let version: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => Ok(None),
            1 => row(&c, id),
            v => bail!("openings.sqlite is of version {v}, newer than this hub knows"),
        }
    }

    pub fn row(&self, id: &[u8; 32]) -> Result<Option<Row>> {
        row(&self.0, id)
    }

    pub fn signed_ids(&self) -> Result<Vec<[u8; 32]>> {
        let mut stmt = self.0.prepare("SELECT id FROM openings WHERE irrevocable = 1 AND abandoned = 0 ORDER BY created_at, rowid")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A new opening, of the holder's deposit `deposit`: false if another
    /// opening not given up spends it. What the hub charges, if told, is
    /// kept with it in the same write.
    pub fn insert(
        &self,
        id: &[u8; 32],
        phase: u8,
        mock: bool,
        deposit: &OutPoint,
        last: &[u8; 32],
        state: &[u8],
    ) -> Result<bool> {
        let now = crate::now();
        let t = rusqlite::Transaction::new_unchecked(&self.0, TransactionBehavior::Immediate)?;
        let inserted = t.execute(
            "INSERT INTO openings (id, format, phase, mock, irrevocable, deposit, coins, last,
                                   state, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, x'', ?6, ?7, ?8, ?8)
             ON CONFLICT DO NOTHING",
            params![
                id,
                FORMAT,
                phase,
                mock,
                serialize(deposit),
                last,
                state,
                now
            ],
        )?;
        if let Some(fees) = self.1.as_ref().filter(|_| inserted == 1) {
            t.execute_batch(FEES)?;
            t.execute(
                "INSERT INTO fees (id, fees, created_at) VALUES (?1, ?2, ?3)",
                params![id, serde_json::to_string(fees)?, now],
            )?;
        }
        t.commit()?;
        Ok(inserted == 1)
    }

    /// The opening `id` after a step taking the message of digest `last`:
    /// `irrevocable` once the hub signed F.
    pub fn save(
        &self,
        id: &[u8; 32],
        phase: u8,
        irrevocable: bool,
        last: &[u8; 32],
        state: &[u8],
    ) -> Result<()> {
        self.0.execute(
            "UPDATE openings SET phase = ?2, irrevocable = ?3, last = ?4, state = ?5,
                                 updated_at = ?6
             WHERE id = ?1",
            params![id, phase, irrevocable, last, state, crate::now()],
        )?;
        Ok(())
    }

    /// The hub's own coins the F of opening `id` spends.
    pub fn set_coins(&self, id: &[u8; 32], coins: &[OutPoint]) -> Result<()> {
        let bytes: Vec<u8> = coins.iter().flat_map(serialize).collect();
        self.0.execute(
            "UPDATE openings SET coins = ?2 WHERE id = ?1",
            params![id, bytes],
        )?;
        Ok(())
    }

    /// How many openings are under way: not given up, the hub's
    /// signatures not given.
    pub fn under_way(&self) -> Result<usize> {
        Ok(self.0.query_row(
            "SELECT count(*) FROM openings WHERE abandoned = 0 AND irrevocable = 0",
            [],
            |r| r.get::<_, i64>(0),
        )? as usize)
    }

    /// Candidate openings untouched since `before`. Recheck when abandoning.
    pub fn stale(&self, before: i64) -> Result<Vec<[u8; 32]>> {
        let mut stmt = self.0.prepare(
            "SELECT id FROM openings
             WHERE abandoned = 0 AND irrevocable = 0 AND updated_at < ?1",
        )?;
        let rows = stmt.query_map([before], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Give up a still-idle, unsigned opening and return only its current
    /// coins. A stale selection never authorizes unlocking a signed row.
    pub fn abandon(&self, id: &[u8; 32], before: i64) -> Result<Option<Vec<OutPoint>>> {
        let coins: Option<Vec<u8>> = self
            .0
            .query_row(
                "UPDATE openings SET abandoned = 1, updated_at = ?2
             WHERE id = ?1 AND abandoned = 0 AND irrevocable = 0 AND updated_at < ?3
             RETURNING coins",
                params![id, crate::now(), before],
                |r| r.get(0),
            )
            .optional()?;
        coins.map(|coins| outpoints(&coins)).transpose()
    }

    /// The coins of the openings not given up: locked in the hub's node.
    pub fn locked(&self) -> Result<Vec<OutPoint>> {
        let mut stmt = self
            .0
            .prepare("SELECT coins FROM openings WHERE abandoned = 0")?;
        let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
        let mut all = Vec::new();
        for coins in rows {
            all.extend(outpoints(&coins?)?);
        }
        Ok(all)
    }
}

fn row(c: &Connection, id: &[u8; 32]) -> Result<Option<Row>> {
    let row = c
        .query_row(
            "SELECT format, state, last, abandoned FROM openings WHERE id = ?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    Row {
                        state: r.get(1)?,
                        last: r.get(2)?,
                        abandoned: r.get(3)?,
                    },
                ))
            },
        )
        .optional()?;
    match row {
        Some((FORMAT, row)) => Ok(Some(row)),
        Some((format, _)) => bail!("an opening stored in format {format}, unknown"),
        None => Ok(None),
    }
}

fn outpoints(bytes: &[u8]) -> Result<Vec<OutPoint>> {
    bytes.chunks(36).map(|c| Ok(deserialize(c)?)).collect()
}
