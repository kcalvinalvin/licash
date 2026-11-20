//! What the hub checks of a claim a contract is folded on, before it signs
//! the fold that pays it ([`crate::FOLD_CLAIM`]).
//!
//! A fold has no dispute: nothing is proven after it and no spend is cited,
//! so the hub pays out of the contract's collateral only for notes it
//! retires first and that nothing spent. Of the claim it takes settled
//! notes alone, worth the amount it pays or more (the cap leaves a note
//! beyond it paid for what it has room for, as in a forced close), and
//! owed all of it by the statement the dispute would decide on, with no
//! evidence: what the ledger holds against the notes is its own to say
//! ([`FoldLedger`]). Locks and fallbacks it leaves to a forced close.

use std::collections::BTreeSet;

use anyhow::{ensure, Context as _, Result};
use licash::claim::Item;
use licash::crypto::Point;
use licash::recovery::{self, Filing, Format, Publication};
use licash::request::Request;
use licash_contract::opening::HubOpening;

/// The hub's ledger, as a fold reads and retires the notes of a claim
/// ([`super::Openings::folds_with`]).
pub struct FoldLedger {
    /// Whether the notes may be claimed by the fold of a contract: none
    /// spent, none claimed in another claim. Nothing is retired.
    pub claimable: Box<Claimable>,
    /// Retire the notes of a claim for a contract's fold, if none was
    /// spent or claimed in another claim, all or none. Again is nothing
    /// new.
    pub retire: Box<Retire>,
}

/// Retire the notes of a claim for a contract's fold ([`FoldLedger::retire`]).
pub type Retire = dyn Fn(&[u8; 32], &Publication) -> Result<()> + Send + Sync;

/// Whether notes are open to a contract's fold ([`FoldLedger::claimable`]).
pub type Claimable = dyn Fn(&[u8; 32], &[Point]) -> Result<()> + Send + Sync;

/// What a holder asked of a fold, to be filed as she asked: the amount and
/// the notes it is made of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Claim {
    pub amount: u32,
    pub notes: Vec<Point>,
}

impl Claim {
    /// What the ask names, within what `o`'s contract can pay.
    pub(super) fn asked(o: &HubOpening, amount: u32, notes: Vec<Point>) -> Result<Self> {
        let parts = o.base.parts();
        ensure!(amount > 0, "a claim of nothing");
        ensure!(
            amount <= parts.cap,
            "a claim of {amount} sat, above its cap {}",
            parts.cap
        );
        let distinct: BTreeSet<_> = notes.iter().map(|y| y.0).collect();
        ensure!(distinct.len() == notes.len(), "a note named twice");
        Ok(Self { amount, notes })
    }

    /// What it pays her, in sat.
    pub(super) fn sats(&self, o: &HubOpening) -> bitcoin::Amount {
        bitcoin::Amount::from_sat(u64::from(self.amount) * o.base.parts().graph.unit_sats)
    }

    /// Her claim as filed in `bytes` ([`Filing::to_bytes`]), if it is the
    /// one asked and owed all of it: of settled notes of the hub's
    /// keyset, nothing else, naming the notes asked and worth the amount
    /// or more, eligible under her opening. The statement's own decision
    /// ([`recovery::owed`]), with no evidence: whether the ledger holds
    /// any is the ledger's to say ([`FoldLedger`]).
    pub(super) fn filed(&self, o: &HubOpening, bytes: &[u8]) -> Result<Publication> {
        let parts = o.base.parts();
        let limit = parts.limits.max_authorizations as usize;
        let filing = Filing::from_bytes(Format::of(&o.base), &parts.keyset, limit, bytes)
            .context("her filing")?;
        let publication = filing.publication();
        let claim = publication.claim();
        ensure!(
            claim.amount() == self.amount,
            "her claim is of {} sat, not the {} asked",
            claim.amount(),
            self.amount
        );
        let items = &claim.parts().items;
        ensure!(
            claim.parts().sources.is_empty()
                && claim.parts().activations.is_empty()
                && items.iter().all(|i| matches!(i, Item::Settled(_))),
            "a fold takes settled notes alone: locks go to a forced close"
        );
        let mut named = BTreeSet::new();
        let mut worth = 0u64;
        for item in items {
            named.insert(item.message().identifier()?.0);
            worth += item.message().denomination();
        }
        let asked: BTreeSet<_> = self.notes.iter().map(|y| y.0).collect();
        ensure!(named == asked, "her claim names other notes than asked");
        // As a forced close claims: a note beyond what is left of the cap is
        // retired whole and paid for what the cap has room for.
        ensure!(
            worth >= u64::from(self.amount),
            "her notes are worth {worth} sat, less than the {} claimed",
            self.amount
        );
        let request = Request {
            amount: self.amount,
            claim_digest: *claim.digest(),
        };
        let chain = licash_hp::chain(&o.base, &[]).map_err(|e| anyhow::anyhow!("{e}"))?;
        let owed = recovery::owed(&o.base, &request, bytes, &[], &chain)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        ensure!(
            owed.ineligible.is_none() && owed.owed == self.amount,
            "her claim is owed {} of {} sat{}",
            owed.owed,
            self.amount,
            owed.ineligible
                .map(|why| format!(": {why}"))
                .unwrap_or_default()
        );
        Ok(publication.clone())
    }
}
