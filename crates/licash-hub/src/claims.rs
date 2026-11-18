//! Holders' claims on Bitcoin, as the ledger keeps them.
//!
//! A claim published in a recovery contract names notes, each by its hold
//! key signing the claim. Once the hub reads a claim, no spend takes a note
//! it names, ever: whatever her dispute pays her, she gave the notes up for
//! it. Each claimed note is also evidence against any other claim of it
//! ([`Evidence::SecondClaim`]), so no note is paid twice across contracts.

use licash::{
    crypto::{Point, Signature},
    hold::{self, Record},
    note::Message,
    recovery::{Evidence, Publication},
};
use rusqlite::{params, Connection};

use crate::{store::now, Error, Hub};

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
            evidence.push(Evidence::SecondClaim {
                context: hold::Context::from_bytes(&context),
                record: Record {
                    message: Message::from_bytes(&message)?,
                    sig: Signature(sig),
                },
            });
        }
    }
    Ok(evidence)
}
