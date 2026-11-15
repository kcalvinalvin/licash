//! Notes claimed on Bitcoin: taken by a contract's force close out of every
//! payment, as a payment takes the notes it spends, and what authorizes
//! them in the claim.
//!
//! A claim's notes are taken under the contract's id: no send, lock,
//! renewal or unlink selects them from then on, and the balance counts
//! them no more (the contract's recovery shows them). They are given back
//! only while the recovery can still be given up, before the holder signs
//! her request on Bitcoin; after, they stay taken for good.

use licash::{
    claim::Branch,
    crypto::{Point, ScalarBytes},
    note::{Keyset, Message, Note},
};
use rusqlite::{params, OptionalExtension};

use crate::{held, Error, Held, Transport, Wallet};

/// A note a contract's recovery claims, as the wallet keeps it: the note,
/// and what derives its keys again (its proposal's ephemeral key, its
/// branch and its position there).
#[derive(Clone, Debug)]
pub struct ClaimNote {
    pub note: Note,
    pub ephemeral: [u8; 33],
    pub branch: Branch,
    pub position: u32,
}

impl ClaimNote {
    /// The note's bytes and its coordinates, for the recovery to keep a
    /// copy of what it claimed.
    pub fn to_bytes(&self) -> Vec<u8> {
        let note = self.note.to_bytes();
        let mut bytes = Vec::with_capacity(4 + note.len() + 33 + 1 + 4);
        bytes.extend((note.len() as u32).to_be_bytes());
        bytes.extend(note);
        bytes.extend(self.ephemeral);
        bytes.push(match self.branch {
            Branch::Pay => 0,
            Branch::Refund => 1,
        });
        bytes.extend(self.position.to_be_bytes());
        bytes
    }

    /// [`ClaimNote::to_bytes`] read back, of notes of `keyset`.
    pub fn from_bytes(keyset: &Keyset, bytes: &[u8]) -> Result<Self, Error> {
        let bad = || Error::Invalid("a claimed note's bytes".into());
        let (len, rest) = bytes.split_first_chunk::<4>().ok_or_else(bad)?;
        let len = u32::from_be_bytes(*len) as usize;
        if rest.len() != len + 33 + 1 + 4 {
            return Err(bad());
        }
        let (note, rest) = rest.split_at(len);
        let (ephemeral, rest) = rest.split_first_chunk::<33>().ok_or_else(bad)?;
        let (branch, position) = rest.split_first().ok_or_else(bad)?;
        Ok(Self {
            note: Note::from_bytes(keyset, note)?,
            ephemeral: *ephemeral,
            branch: match branch {
                0 => Branch::Pay,
                1 => Branch::Refund,
                _ => return Err(bad()),
            },
            position: u32::from_be_bytes(position.try_into().map_err(|_| bad())?),
        })
    }

    fn held(&self) -> Held {
        Held {
            note: self.note.clone(),
            ephemeral: Point(self.ephemeral),
            branch: self.branch,
            position: self.position,
        }
    }
}

impl<T: Transport> Wallet<T> {
    /// Take for the recovery of contract `id` the wallet's spendable notes
    /// of the keyset of id `keyset`: the largest first, as long as they
    /// come to `cap` at most, `max` of them at most. All or none: none if
    /// a payment took one meanwhile. None at all if the recovery took
    /// notes already: [`Wallet::claimed_notes`] are its own.
    pub fn claim_notes(
        &self,
        id: &[u8; 32],
        keyset: &[u8; 32],
        cap: u64,
        max: usize,
    ) -> Result<Vec<ClaimNote>, Error> {
        let keyset = self.keyset_of(keyset)?;
        self.store.write(|t| {
            let taken: Option<i64> = t
                .query_row(
                    "SELECT 1 FROM notes WHERE spent_by = ?1 LIMIT 1",
                    [id],
                    |r| r.get(0),
                )
                .optional()?;
            if taken.is_some() {
                return Err(Error::Invalid(
                    "this recovery took its notes already".into(),
                ));
            }
            let mut stmt = t.prepare(
                "SELECT y, denom FROM notes
                 WHERE keyset = ?1 AND spent_by IS NULL AND session IS NULL AND gone_at IS NULL
                 ORDER BY denom DESC, y",
            )?;
            let rows = stmt.query_map([keyset.id()], |r| {
                Ok((r.get::<_, [u8; 33]>(0)?, r.get::<_, i64>(1)? as u64))
            })?;
            let (mut ys, mut sum) = (Vec::new(), 0u64);
            for row in rows {
                let (y, denom) = row?;
                if ys.len() == max {
                    break;
                }
                if sum + denom <= cap {
                    sum += denom;
                    ys.push(y);
                }
            }
            drop(stmt);
            let mut notes = Vec::with_capacity(ys.len());
            for y in &ys {
                let taken = t.execute(
                    "UPDATE notes SET spent_by = ?1
                     WHERE y = ?2 AND spent_by IS NULL AND session IS NULL AND gone_at IS NULL",
                    params![id, y],
                )?;
                if taken != 1 {
                    return Err(Error::NoteTaken);
                }
                notes.push(claim_note(t, &keyset, y)?);
            }
            Ok(notes)
        })
    }

    /// The notes the recovery of contract `id` took, of the keyset of id
    /// `keyset`, the largest first.
    pub fn claimed_notes(&self, id: &[u8; 32], keyset: &[u8; 32]) -> Result<Vec<ClaimNote>, Error> {
        let keyset = self.keyset_of(keyset)?;
        self.store.read(|c| {
            let mut stmt = c.prepare(
                "SELECT y FROM notes WHERE spent_by = ?1 AND keyset = ?2 ORDER BY denom DESC, y",
            )?;
            let ys = stmt
                .query_map(params![id, keyset.id()], |r| r.get::<_, [u8; 33]>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            ys.iter().map(|y| claim_note(c, &keyset, y)).collect()
        })
    }

    /// Give back the notes the recovery of contract `id` took, for it was
    /// given up before its request was signed: how many.
    pub fn unclaim_notes(&self, id: &[u8; 32]) -> Result<usize, Error> {
        let freed = self.store.write(|t| {
            Ok(t.execute("UPDATE notes SET spent_by = NULL WHERE spent_by = ?1", [id])?)
        })?;
        tracing::info!(id = %hex::encode(id), freed, "claimed notes given back");
        Ok(freed)
    }

    /// Each of `notes`' message and its hold key, which authorizes it in a
    /// claim: of this wallet's keys, refused for a note not derived from
    /// them.
    pub fn hold_keys(&self, notes: &[ClaimNote]) -> Result<Vec<(Message, ScalarBytes)>, Error> {
        notes
            .iter()
            .map(|n| {
                let derivation = self.derivation(&n.held())?;
                if derivation.message() != n.note.message() {
                    return Err(Error::Invalid("a note not of this wallet's keys".into()));
                }
                let key = derivation.hold_key(self.owner.hold_root())?;
                Ok((n.note.message().clone(), key))
            })
            .collect()
    }

    /// The wallet's keyset of id `id`.
    fn keyset_of(&self, id: &[u8; 32]) -> Result<Keyset, Error> {
        let bytes: Option<Vec<u8>> = self.store.read(|c| {
            Ok(
                c.query_row("SELECT bytes FROM keysets WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .optional()?,
            )
        })?;
        let bytes = bytes.ok_or_else(|| {
            Error::Invalid(format!(
                "keyset {}: not one of the wallet's",
                hex::encode(id)
            ))
        })?;
        Ok(Keyset::from_bytes(&bytes)?)
    }
}

fn claim_note(c: &rusqlite::Connection, keyset: &Keyset, y: &[u8; 33]) -> Result<ClaimNote, Error> {
    let held = held(c, keyset, y)?;
    Ok(ClaimNote {
        note: held.note,
        ephemeral: held.ephemeral.0,
        branch: held.branch,
        position: held.position,
    })
}
