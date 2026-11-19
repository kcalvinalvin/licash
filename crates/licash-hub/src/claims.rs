//! Holders' claims on Bitcoin, as the ledger keeps them.
//!
//! A claim published in a recovery contract names notes, each by its hold
//! key signing the claim. Once the hub reads a claim, no spend takes a note
//! it names, ever: whatever her dispute pays her, she gave the notes up for
//! it. Each claimed note is also evidence against any other claim of it
//! ([`Evidence::SecondClaim`]), so no note is paid twice across contracts.
//! A claim a contract is closed cooperatively on ([`Hub::claim_for_fold`])
//! retires its notes the same way, before the hub signs the fold that pays
//! them, but only of notes nothing spent: no dispute follows to cite one.

use licash::{
    crypto::{Point, Signature},
    hold::{self, Record},
    note::Message,
    recovery::{Evidence, Publication},
};
use rusqlite::{params, Connection};

use crate::{store::now, Error, Hub, Refusal};

impl Hub {
    /// Retire the notes that `publication`, the claim filed in the contract
    /// `contract`, names: from now on no spend takes them. Their hold keys'
    /// signatures were checked as it was decoded, so a claim retires only
    /// notes its holder can sign for. Retiring them again changes nothing.
    pub fn retire_claimed(
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
        let held = self.store.write(|tx| {
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
            tracing::info!(contract = %hex::encode(contract), notes = held, "claimed notes retired");
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
        if spent.exists([&y.0])? {
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

/// Whether a claim on Bitcoin retired `y`.
pub(crate) fn is_claimed(c: &Connection, y: &Point) -> Result<bool, Error> {
    Ok(c.prepare_cached("SELECT 1 FROM claimed WHERE y = ?1")?
        .exists([&y.0])?)
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
/// key's signature of that claim, by note then contract.
pub(crate) fn second_claims(
    c: &Connection,
    ys: &[Point],
    context: &hold::Context,
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
            let (context, message, sig) = row?;
            evidence.push(second_claim(&context, &message, sig)?);
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
    let mut evidence = Vec::new();
    for row in rows {
        let (context, message, sig) = row?;
        evidence.push(second_claim(&context, &message, sig)?);
    }
    Ok(evidence)
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
