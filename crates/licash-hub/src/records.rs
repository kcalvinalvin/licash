//! What a hub's ledger records over time, read back for its operator
//! ([`Ledger`]): notes issued and retired, locks, payments outside the
//! ledger and the chain as scanned. Read-only, as `Ledger` is.

use std::collections::{HashMap, HashSet};

use licash::note::Note;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::{Error, Ledger};

/// A note issued or retired, when the ledger recorded it. Summed in order,
/// these give the notes outstanding since the ledger began: in holders'
/// hands, the hub's own (`hub`), or behind a lock, which is not a note yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Movement {
    /// Unix seconds.
    pub at: u64,
    /// The keyset's index.
    pub keyset: u32,
    pub denom: u64,
    /// Issued, else retired.
    pub issued: bool,
    /// A note paid to the hub: its income.
    pub hub: bool,
    /// A retired note whose evidence was of an unlinked issuance.
    pub unlinked: bool,
}

/// A lock as admitted, and settled if it was.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockRecord {
    pub id: [u8; 32],
    pub amount: u64,
    /// Of notes the hub issued itself, backed by what the caller holds.
    pub own: bool,
    pub expiry: u32,
    /// Unix seconds.
    pub created_at: u64,
    /// `Some(true)` paid, `Some(false)` refunded.
    pub paid: Option<bool>,
    pub settled_at: Option<u64>,
}

/// A payment the hub made outside its ledger for a lock that pays it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardRecord {
    pub lock: [u8; 32],
    /// The request it paid, such as a BOLT 11 invoice.
    pub request: String,
    /// The lock's amount.
    pub amount: u64,
    pub expiry: u32,
    /// `pending`, `paid` or `failed`.
    pub state: String,
    pub reason: Option<String>,
    pub created_at: u64,
    pub resolved_at: Option<u64>,
}

/// Of one keyset's notes of one denomination: how many outputs the hub
/// answered an unlink for, and how many unlinked notes were retired since.
/// Their difference bounds the unlinked notes outstanding from above: an
/// answer does not prove its holder finished unlinking.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unlinks {
    pub keyset: u32,
    pub denom: u64,
    pub answered: u64,
    pub retired: u64,
}

impl Unlinks {
    fn none(keyset: u32, denom: u64) -> Self {
        Self {
            keyset,
            denom,
            answered: 0,
            retired: 0,
        }
    }
}

/// The ledger's notes over time ([`Ledger::notes`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notes {
    pub movements: Vec<Movement>,
    /// By keyset then denomination.
    pub unlinks: Vec<Unlinks>,
}

fn id(bytes: Vec<u8>) -> Result<[u8; 32], Error> {
    bytes
        .try_into()
        .map_err(|_| Error::Setup("a ledger id of the wrong length".into()))
}

impl Ledger {
    /// When the ledger began, Unix seconds.
    pub fn created_at(&self) -> Result<u64, Error> {
        Ok(self
            .conn
            .query_row("SELECT created_at FROM meta", [], |r| r.get::<_, i64>(0))?
            as u64)
    }

    /// The height of the best chain as last scanned, if any.
    pub fn tip(&self) -> Result<Option<u32>, Error> {
        Ok(self
            .conn
            .query_row("SELECT max(height) FROM blocks", [], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .optional()?
            .flatten()
            .map(|h| h as u32))
    }

    /// Every note issued and retired, oldest first, and the unlinks
    /// answered, read at one moment. A grant's notes move when it was
    /// released; a lock's inputs when it was admitted, its branch's
    /// outputs when it settled. An own lock's inputs were never notes, so
    /// they do not move.
    pub fn notes(&self) -> Result<Notes, Error> {
        let tx = self.conn.unchecked_transaction()?;
        let keysets: HashMap<Vec<u8>, u32> = {
            let mut stmt = tx.prepare("SELECT id, idx FROM keysets")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u32)))?;
            rows.collect::<Result<_, _>>()?
        };
        let index = |k: &[u8]| -> Result<u32, Error> {
            keysets
                .get(k)
                .copied()
                .ok_or_else(|| Error::Setup("a note of an unknown keyset".into()))
        };
        // The hub's own notes: by identifier, and by where each was issued.
        let mut income: HashSet<Vec<u8>> = HashSet::new();
        let mut paid_in: HashSet<(Vec<u8>, i64)> = HashSet::new();
        {
            let mut stmt = tx.prepare("SELECT proposal, position, note FROM income")?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                paid_in.insert((r.get(0)?, r.get(1)?));
                let peek = Note::peek(&r.get::<_, Vec<u8>>(2)?)?;
                income.insert(peek.message.identifier()?.0.to_vec());
            }
        }
        let mut movements = Vec::new();
        let mut sets: HashMap<(u32, u64), Unlinks> = HashMap::new();
        {
            let mut stmt = tx.prepare(
                "SELECT i.keyset, i.denom, i.proposal, i.position, i.answered IS NOT NULL,
                        coalesce(g.released_at, l.settled_at)
                 FROM issued i
                 LEFT JOIN grants g ON g.id = i.proposal
                 LEFT JOIN locks l ON l.id = i.proposal
                 WHERE i.boot IS NOT NULL",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let keyset = index(&r.get::<_, Vec<u8>>(0)?)?;
                let denom = r.get::<_, i64>(1)? as u64;
                if r.get::<_, bool>(4)? {
                    sets.entry((keyset, denom))
                        .or_insert_with(|| Unlinks::none(keyset, denom))
                        .answered += 1;
                }
                let Some(at) = r.get::<_, Option<i64>>(5)? else {
                    continue;
                };
                movements.push(Movement {
                    at: at as u64,
                    keyset,
                    denom,
                    issued: true,
                    hub: paid_in.contains(&(r.get(2)?, r.get(3)?)),
                    unlinked: false,
                });
            }
        }
        {
            let mut stmt = tx.prepare(
                "SELECT s.y, s.note, coalesce(g.released_at, p.created_at)
                 FROM spent s
                 JOIN proposals p ON p.id = s.proposal
                 LEFT JOIN grants g ON g.id = s.proposal
                 LEFT JOIN locks l ON l.id = s.proposal
                 WHERE coalesce(l.own, 0) = 0",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let Some(at) = r.get::<_, Option<i64>>(2)? else {
                    continue;
                };
                let peek = Note::peek(&r.get::<_, Vec<u8>>(1)?)?;
                let (keyset, denom) = (index(&peek.keyset)?, peek.message.denomination());
                if peek.unlinked {
                    sets.entry((keyset, denom))
                        .or_insert_with(|| Unlinks::none(keyset, denom))
                        .retired += 1;
                }
                movements.push(Movement {
                    at: at as u64,
                    keyset,
                    denom,
                    issued: false,
                    hub: income.contains(&r.get::<_, Vec<u8>>(0)?),
                    unlinked: peek.unlinked,
                });
            }
        }
        movements.sort_by_key(|m| (m.at, !m.issued));
        let mut unlinks: Vec<Unlinks> = sets.into_values().collect();
        unlinks.sort_by_key(|u| (u.keyset, u.denom));
        Ok(Notes { movements, unlinks })
    }

    /// Every lock, oldest first.
    pub fn locks(&self) -> Result<Vec<LockRecord>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT l.id, l.amount, l.own, l.expiry, p.created_at, l.settled, l.settled_at
             FROM locks l JOIN proposals p ON p.id = l.id ORDER BY p.created_at, l.rowid",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                LockRecord {
                    id: [0; 32],
                    amount: r.get::<_, i64>(1)? as u64,
                    own: r.get::<_, i64>(2)? != 0,
                    expiry: r.get::<_, i64>(3)? as u32,
                    created_at: r.get::<_, i64>(4)? as u64,
                    paid: r.get::<_, Option<String>>(5)?.map(|s| s == "pay"),
                    settled_at: r.get::<_, Option<i64>>(6)?.map(|t| t as u64),
                },
            ))
        })?;
        rows.map(|row| {
            let (bytes, lock) = row?;
            Ok(LockRecord {
                id: id(bytes)?,
                ..lock
            })
        })
        .collect()
    }

    /// Every payment outside the ledger, oldest first.
    pub fn forwards(&self) -> Result<Vec<ForwardRecord>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT lock, request, amount, expiry, state, reason, created_at, resolved_at
             FROM forwards ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                ForwardRecord {
                    lock: [0; 32],
                    request: r.get(1)?,
                    amount: r.get::<_, i64>(2)? as u64,
                    expiry: r.get::<_, i64>(3)? as u32,
                    state: r.get(4)?,
                    reason: r.get(5)?,
                    created_at: r.get::<_, i64>(6)? as u64,
                    resolved_at: r.get::<_, Option<i64>>(7)?.map(|t| t as u64),
                },
            ))
        })?;
        rows.map(|row| {
            let (bytes, forward) = row?;
            Ok(ForwardRecord {
                lock: id(bytes)?,
                ..forward
            })
        })
        .collect()
    }
}
