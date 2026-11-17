//! What the ledger holds against a holder's recovery claim.
//!
//! A claim names notes (its items' identifiers) and the sources of its
//! pending items (their inputs' identifiers, their timed terms). The hub
//! knows of each identifier it retired the proposal that spent it, signed
//! by the notes' owners, and of each timed source which branch it settled.
//! These are candidates only: [`licash::recovery::owed`] decides which void
//! anything. The hub cites against a timed source only the branch it did
//! not pay: a lock it refunded is not voided by its preimage, one it paid
//! not by its expiry.

use std::collections::BTreeSet;

use licash::{
    authorization::{Authorization, SignedProposal},
    claim::UnsignedClaim,
    crypto::Point,
    note::Note,
    recovery::Evidence,
    spend::ProposalId,
};
use rusqlite::{Connection, OptionalExtension};

use crate::{Error, Hub};

impl Hub {
    /// The evidence the ledger holds against `claim`: each owner-signed
    /// spend that retired one of its notes or one of its sources' inputs,
    /// a granted spend with its unlock (released, so its payee has it), and
    /// against each of its timed sources the branch the hub did not settle
    /// it on: the preimage of a lock paid, the expiry of one refunded, of
    /// one unsettled the preimage if the hub saw it, else its expiry, which
    /// the statement checks against the chain. Ordered by proposal, then
    /// source. A spend the ledger can no longer read back (its keyset gone)
    /// is left out.
    pub fn evidence(&self, claim: &UnsignedClaim) -> Result<Vec<Evidence>, Error> {
        let parts = claim.parts();
        let mut ys = Vec::new();
        for item in &parts.items {
            ys.push(item.message().identifier()?);
        }
        for source in &parts.sources {
            ys.extend(source.inputs().iter().copied());
        }
        self.store.read(|c| {
            let mut evidence = Vec::new();
            for id in spenders(c, &ys)? {
                match self.spend(c, &id) {
                    Ok(Some(spend)) => evidence.push(spend),
                    Ok(None) => {}
                    Err(e) => tracing::warn!(
                        proposal = %hex::encode(id.as_bytes()),
                        "a retired spend not cited against a claim: {e}"
                    ),
                }
            }
            for source in &parts.sources {
                let Some(terms) = source.terms() else {
                    continue;
                };
                let id = source.id();
                let settled: Option<String> = c
                    .query_row(
                        "SELECT settled FROM locks WHERE id = ?1",
                        [id.as_bytes()],
                        |r| r.get(0),
                    )
                    .optional()?
                    .flatten();
                let seen = preimage(c, terms.hash())?;
                evidence.extend(against_timed(id, settled.as_deref(), seen));
            }
            Ok(evidence)
        })
    }

    /// The spend `id` as its owners signed it, with the notes it retired in
    /// order and, granted, its unlock: none if it is neither a lock nor a
    /// released grant.
    fn spend(&self, c: &Connection, id: &ProposalId) -> Result<Option<Evidence>, Error> {
        let signed: Option<Vec<u8>> = c
            .query_row(
                "SELECT signed FROM locks WHERE id = ?1
                 UNION ALL
                 SELECT signed FROM grants WHERE id = ?1 AND released_at IS NOT NULL",
                [id.as_bytes()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(signed) = signed else {
            return Ok(None);
        };
        let keyset_id: [u8; 32] = c.query_row(
            "SELECT keyset FROM proposals WHERE id = ?1",
            [id.as_bytes()],
            |r| r.get(0),
        )?;
        let (_, keyset) = self.keys(&keyset_id)?;
        let mut stmt =
            c.prepare_cached("SELECT note FROM spent WHERE proposal = ?1 ORDER BY position")?;
        let notes = stmt
            .query_map([id.as_bytes()], |r| r.get::<_, Vec<u8>>(0))?
            .map(|bytes| Ok(Note::from_bytes(keyset, &bytes?)?))
            .collect::<Result<Vec<_>, Error>>()?;
        let signed = SignedProposal::from_bytes(keyset, &notes, &signed)?;
        let unlock = match signed.authorization() {
            Authorization::Plain => None,
            Authorization::Granted(_) => {
                let salt: [u8; 32] = c.query_row(
                    "SELECT salt FROM grants WHERE id = ?1",
                    [id.as_bytes()],
                    |r| r.get(0),
                )?;
                Some(self.unlock(&keyset_id, id, &salt))
            }
        };
        Ok(Some(Evidence::Spend {
            signed,
            notes,
            unlock,
        }))
    }
}

/// Against the timed source `id`, settled on `settled` (`pay`, `refund`,
/// none) with `seen` the preimage of its hash the hub saw, if any: the
/// branch the hub did not pay. A lock refunded may share its hash with
/// one activated, or be activated after its expiry: its preimage is never
/// cited. One paid without a preimage known has nothing against it.
fn against_timed(
    id: ProposalId,
    settled: Option<&str>,
    seen: Option<[u8; 32]>,
) -> Option<Evidence> {
    match (settled, seen) {
        (Some("refund"), _) | (None, None) => Some(Evidence::Expired { source: id }),
        (_, Some(preimage)) => Some(Evidence::PayPreimage {
            source: id,
            preimage,
        }),
        (_, None) => None,
    }
}

/// The proposals that retired any of `ys`, each once, by id.
fn spenders(c: &Connection, ys: &[Point]) -> Result<BTreeSet<ProposalId>, Error> {
    let mut stmt = c.prepare_cached("SELECT proposal FROM spent WHERE y = ?1")?;
    let mut ids = BTreeSet::new();
    for y in ys {
        if let Some(id) = stmt
            .query_row([&y.0], |r| r.get::<_, [u8; 32]>(0))
            .optional()?
        {
            ids.insert(ProposalId::from_bytes(id));
        }
    }
    Ok(ids)
}

/// A preimage of `hash` the hub saw: an activation it scanned, or one a
/// lock or a forward was settled with.
fn preimage(c: &Connection, hash: &[u8; 32]) -> Result<Option<[u8; 32]>, Error> {
    Ok(c.query_row(
        "SELECT secret FROM activations WHERE hash = ?1
         UNION ALL SELECT preimage FROM locks WHERE hash = ?1 AND preimage IS NOT NULL
         UNION ALL SELECT preimage FROM forwards WHERE hash = ?1 AND preimage IS NOT NULL
         LIMIT 1",
        [hash],
        |r| r.get(0),
    )
    .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cited(settled: Option<&str>, seen: Option<[u8; 32]>) -> &'static str {
        match against_timed(ProposalId::from_bytes([1; 32]), settled, seen) {
            Some(Evidence::PayPreimage { preimage, .. }) => {
                assert_eq!(Some(preimage), seen);
                "preimage"
            }
            Some(Evidence::Expired { .. }) => "expired",
            Some(_) => "other",
            None => "nothing",
        }
    }

    #[test]
    fn a_timed_source_is_met_on_the_branch_the_hub_did_not_pay() {
        let z = Some([2; 32]);
        // Paid: its preimage voids the refund she claims.
        assert_eq!(cited(Some("pay"), z), "preimage");
        assert_eq!(cited(Some("pay"), None), "nothing");
        // Refunded: its expiry voids the pay she claims, whatever
        // activation of its hash the hub saw.
        assert_eq!(cited(Some("refund"), z), "expired");
        assert_eq!(cited(Some("refund"), None), "expired");
        // Unsettled: the preimage if seen, else its expiry, which the
        // statement checks on the chain.
        assert_eq!(cited(None, z), "preimage");
        assert_eq!(cited(None, None), "expired");
    }
}
