//! Holders' claims on Bitcoin, as the ledger keeps them.
//!
//! A claim published in a recovery contract names notes, each by its hold
//! key signing the claim. Once the hub reads a claim, no spend takes a note
//! it names so, ever: whatever her dispute pays her, she gave the notes up
//! for it. Each claimed note is also evidence against any other claim of it
//! ([`Evidence::SecondClaim`]), so no note is paid twice across contracts.
//! A note named with a signature that is not its hold key's is not claimed:
//! nobody gave it up, and the row would cite nothing.
//!
//! One exception, of a lean contract, whose statement verifies no hold
//! signature of the inputs of a lock of her own notes
//! ([`licash::claim::Source::OwnInputs`]): it owes her the refund of a lock
//! the hub never served whether or not their hold keys signed her claim.
//! Such an input is given up by the lock itself, which its owner signed
//! ([`licash::recovery::Named::locks`]): the hub keeps the lock, spends the
//! note for nobody, and cites the lock, an owner-signed spend, against any
//! other claim of it. The lock is not served: neither branch of it is ever
//! issued.
//!
//! A claim a contract is closed cooperatively on ([`Hub::claim_for_fold`])
//! retires its notes the same way, before the hub signs the fold that pays
//! them, but only of notes nothing spent: no dispute follows to cite one.

use std::collections::BTreeSet;

use licash::{
    crypto::{Point, Signature},
    hold::{self, Record},
    note::Message,
    recovery::{evidence_to_bytes, Evidence, Held, Publication},
    spend::ProposalId,
};
use rusqlite::{params, Connection};

use crate::{store::now, Error, Hub, Refusal};

impl Hub {
    /// Retire the notes `held` of the claim filed in the contract
    /// `contract`: from now on no spend takes them. Each is there by its
    /// hold key's signature of the claim ([`Held`]), so a claim retires
    /// only notes its holder can sign for, and all of those, whatever else
    /// of the claim is not valid. Retiring them again changes nothing.
    ///
    /// With them, in the same transaction, each input of the locks `shown`
    /// (the claim's [`licash::recovery::Named::locks`], owner-signed spends
    /// and nothing else) that is neither held nor spent already, and the
    /// lock that gave it up, kept to be cited.
    pub fn retire_claimed(
        &self,
        contract: &[u8; 32],
        held: &Held,
        shown: &[Evidence],
    ) -> Result<(), Error> {
        let context = held.context().to_bytes();
        let records = held
            .records()
            .iter()
            .map(|record| Ok((record.message.identifier()?, record)))
            .collect::<Result<Vec<_>, Error>>()?;
        let by_hold: BTreeSet<Point> = records.iter().map(|(y, _)| *y).collect();
        let mut locks = Vec::new();
        for lock in shown {
            let Evidence::Spend {
                signed,
                notes,
                unlock: None,
            } = lock
            else {
                continue;
            };
            let mut ys = Vec::new();
            for note in notes {
                let y = note.message().identifier()?;
                if !by_hold.contains(&y) {
                    ys.push(y);
                }
            }
            let proposal = signed.proposal();
            let bytes = evidence_to_bytes(std::slice::from_ref(lock));
            locks.push((proposal.id(), *proposal.keyset_id(), ys, bytes));
        }
        let (held, by_lock) = self.store.write(|tx| {
            let mut by_lock = 0;
            for (id, keyset, ys, bytes) in &locks {
                let mut fresh = Vec::new();
                for y in ys {
                    let spent = tx
                        .prepare_cached("SELECT 1 FROM spent WHERE y = ?1")?
                        .exists([&y.0])?;
                    if !spent && !is_shown(tx, y)? {
                        fresh.push(y);
                    }
                }
                if fresh.is_empty() {
                    continue;
                }
                tx.execute(
                    "INSERT INTO shown_locks (id, keyset, contract, evidence, at)
                     VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT DO NOTHING",
                    params![id.as_bytes(), keyset, contract, bytes, now()],
                )?;
                for y in fresh {
                    by_lock += tx.execute(
                        "INSERT INTO shown_inputs (y, lock) VALUES (?1, ?2)",
                        params![&y.0, id.as_bytes()],
                    )?;
                }
            }
            let mut held = 0;
            for (y, record) in &records {
                held += tx.execute(
                    "INSERT INTO claimed (contract, y, context, message, sig, at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT DO NOTHING",
                    params![
                        contract,
                        &y.0,
                        &context[..],
                        &record.message.to_bytes()[..],
                        &record.sig.0[..],
                        now(),
                    ],
                )?;
            }
            Ok((held, by_lock))
        })?;
        if held > 0 {
            tracing::info!(contract = %hex::encode(contract), notes = held, "claimed notes retired");
        }
        if by_lock > 0 {
            tracing::warn!(
                contract = %hex::encode(contract),
                notes = by_lock,
                "notes retired that a claim gave up by a lock alone, their hold keys' \
                 signatures not theirs"
            );
        }
        Ok(())
    }
}

impl Hub {
    /// Whether the notes `ys` may be claimed by the cooperative close of
    /// `contract`: none spent, none claimed by another contract. A read:
    /// nothing is retired ([`Hub::claim_for_fold`]).
    pub fn fold_claimable(&self, contract: &[u8; 32], ys: &[Point]) -> Result<(), Error> {
        self.store.read(|c| open_to_fold(c, contract, None, ys))
    }

    /// Retire the notes that `publication`, the claim `contract` is closed
    /// cooperatively on, names, as [`Hub::retire_claimed`] does for one
    /// filed on Bitcoin, but only if none is spent or claimed in another
    /// claim, and in one transaction: the hub pays her for them from the
    /// contract's collateral with no dispute to cite a spend in, so it
    /// refuses any note it spent at all. The same claim again changes
    /// nothing. Retired for good: should the fold not confirm, she files
    /// this claim on Bitcoin.
    pub fn claim_for_fold(
        &self,
        contract: &[u8; 32],
        publication: &Publication,
    ) -> Result<(), Error> {
        let envelope = publication.envelope();
        let context = envelope.context().to_bytes();
        let records = envelope
            .records()
            .iter()
            .map(|record| Ok((record.message.identifier()?, record)))
            .collect::<Result<Vec<_>, Error>>()?;
        let ys: Vec<Point> = records.iter().map(|(y, _)| *y).collect();
        let held = self.store.write(|tx| {
            open_to_fold(tx, contract, Some(&context), &ys)?;
            let mut held = 0;
            for (y, record) in &records {
                held += tx.execute(
                    "INSERT INTO claimed (contract, y, context, message, sig, at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT DO NOTHING",
                    params![
                        contract,
                        &y.0,
                        &context[..],
                        &record.message.to_bytes()[..],
                        &record.sig.0[..],
                        now(),
                    ],
                )?;
            }
            Ok(held)
        })?;
        if held > 0 {
            tracing::info!(contract = %hex::encode(contract), notes = held, "notes retired for a fold");
        }
        Ok(())
    }
}

/// `ys` open to the cooperative close of `contract`: spent by no spend, and
/// claimed by no other contract, nor, with `context`, in any other claim.
fn open_to_fold(
    c: &Connection,
    contract: &[u8; 32],
    context: Option<&[u8; hold::CONTEXT_BYTES]>,
    ys: &[Point],
) -> Result<(), Error> {
    let mut spent = c.prepare_cached("SELECT 1 FROM spent WHERE y = ?1")?;
    let mut claims = c.prepare_cached("SELECT contract, context FROM claimed WHERE y = ?1")?;
    for y in ys {
        if spent.exists([&y.0])? || is_shown(c, y)? {
            return Err(Refusal::Spent.into());
        }
        let rows = claims.query_map([&y.0], |r| {
            Ok((r.get::<_, [u8; 32]>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        for row in rows {
            let (by, filed) = row?;
            // As a spend of a claimed note is refused: spent, for her.
            if &by != contract || context.is_some_and(|own| filed.as_slice() != &own[..]) {
                return Err(Refusal::Spent.into());
            }
        }
    }
    Ok(())
}

/// Whether a claim on Bitcoin retired `y`: by its hold key's signature,
/// or by a lock it showed.
pub(crate) fn is_claimed(c: &Connection, y: &Point) -> Result<bool, Error> {
    Ok(c.prepare_cached("SELECT 1 FROM claimed WHERE y = ?1")?
        .exists([&y.0])?
        || is_shown(c, y)?)
}

/// Whether a claim on Bitcoin gave `y` up by a lock alone.
fn is_shown(c: &Connection, y: &Point) -> Result<bool, Error> {
    Ok(c.prepare_cached("SELECT 1 FROM shown_inputs WHERE y = ?1")?
        .exists([&y.0])?)
}

/// A lock a claim showed, as kept: its keyset and its evidence's bytes.
pub(crate) struct Shown {
    pub(crate) id: ProposalId,
    pub(crate) keyset: [u8; 32],
    pub(crate) evidence: Vec<u8>,
}

/// The locks claims gave any of `ys` up by, each once, in the order of
/// `ys`. A row that is not as it was kept is left out alone, with a
/// warning, and said in `unread`: the others are evidence all the same.
pub(crate) fn shown(
    c: &Connection,
    ys: &[Point],
    unread: &mut Vec<String>,
) -> Result<Vec<Shown>, Error> {
    let mut stmt = c.prepare_cached(
        "SELECT l.id, l.keyset, l.evidence FROM shown_inputs i
         JOIN shown_locks l ON l.id = i.lock WHERE i.y = ?1",
    )?;
    let mut seen = BTreeSet::new();
    let mut locks = Vec::new();
    for y in ys {
        let rows = stmt.query_map([&y.0], |r| {
            Ok(Shown {
                id: ProposalId::from_bytes(r.get(0)?),
                keyset: r.get(1)?,
                evidence: r.get(2)?,
            })
        })?;
        for row in rows {
            match row {
                Ok(lock) => {
                    if seen.insert(lock.id) {
                        locks.push(lock);
                    }
                }
                Err(e) => {
                    let note = hex::encode(y.0);
                    tracing::warn!(%note, "a lock a claim showed not read back: {e}");
                    unread.push(format!("a lock a claim showed of the note {note}: {e}"));
                }
            }
        }
    }
    Ok(locks)
}

/// Whether a claim on Bitcoin retired any of `ys`.
pub(crate) fn claimed(c: &Connection, ys: &[Point]) -> Result<bool, Error> {
    for y in ys {
        if is_claimed(c, y)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Each other claim of any of `ys` than the one of `context`: its hold
/// key's signature of that claim, by note then contract. A row that is not
/// as it was kept is left out alone, with a warning, and said in `unread`:
/// one claim the ledger cannot cite is no reason to cite none.
pub(crate) fn second_claims(
    c: &Connection,
    ys: &[Point],
    context: &hold::Context,
    unread: &mut Vec<String>,
) -> Result<Vec<Evidence>, Error> {
    let own = context.to_bytes();
    let mut stmt = c.prepare_cached(
        "SELECT context, message, sig FROM claimed WHERE y = ?1 AND context != ?2
         ORDER BY contract",
    )?;
    let mut evidence = Vec::new();
    for y in ys {
        let rows = stmt.query_map(params![&y.0, &own[..]], |r| {
            Ok((
                r.get::<_, [u8; hold::CONTEXT_BYTES]>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, [u8; 64]>(2)?,
            ))
        })?;
        for row in rows {
            match cited(y, row) {
                Some(e) => evidence.push(e),
                None => unread.push(format!(
                    "another claim of the note {} does not read back",
                    hex::encode(y.0)
                )),
            }
        }
    }
    Ok(evidence)
}

/// Each claim of the note `y`, whichever: its hold key's signature of it.
pub(crate) fn claims_of(c: &Connection, y: &Point) -> Result<Vec<Evidence>, Error> {
    let mut stmt = c.prepare_cached(
        "SELECT context, message, sig FROM claimed WHERE y = ?1 ORDER BY contract",
    )?;
    let rows = stmt.query_map([&y.0], |r| {
        Ok((
            r.get::<_, [u8; hold::CONTEXT_BYTES]>(0)?,
            r.get::<_, Vec<u8>>(1)?,
            r.get::<_, [u8; 64]>(2)?,
        ))
    })?;
    Ok(rows.filter_map(|row| cited(y, row)).collect())
}

/// A row of `claimed` of the note `y` as evidence: none, with a warning, of
/// one that does not read back.
fn cited(
    y: &Point,
    row: rusqlite::Result<([u8; hold::CONTEXT_BYTES], Vec<u8>, [u8; 64])>,
) -> Option<Evidence> {
    let read = row
        .map_err(Error::from)
        .and_then(|(context, message, sig)| second_claim(&context, &message, sig));
    match read {
        Ok(evidence) => Some(evidence),
        Err(e) => {
            tracing::warn!(note = %hex::encode(y.0), "a claim of a note not cited: {e}");
            None
        }
    }
}

fn second_claim(
    context: &[u8; hold::CONTEXT_BYTES],
    message: &[u8],
    sig: [u8; 64],
) -> Result<Evidence, Error> {
    Ok(Evidence::SecondClaim {
        context: hold::Context::from_bytes(context),
        record: Record {
            message: Message::from_bytes(message)?,
            sig: Signature(sig),
        },
    })
}
