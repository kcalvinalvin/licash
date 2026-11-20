//! Her funded notes issued: the notes she funds her part of a v5
//! contract with, which the hub issues her as its own lock, settled at
//! once, once F is buried and unspent ([`crate::FUNDED`]).
//!
//! Issued by a lock under a preimage of the opening's own, any number of
//! asks issue one lock: the ledger keeps one own lock a hash. The notes
//! pay her outputs, which only she opens, so whoever asks learns nothing
//! of use: the lock and its preimage are hers to claim the notes with.
//! Once her claim on Bitcoin names them, the hub retires those unspent,
//! as it does every note a claim names.

use std::sync::Arc;

use bitcoin::OutPoint;
use licash::crypto::Point;
use licash::spend::Output;
use licash_contract::opening::{Hub, HubPhase};

use super::{Openings, Refused};
use crate::FundedIssued;

/// How deep F is before her funded notes are issued, at the least: one
/// block's reorganization costs the hub their worth.
pub const FUNDED_DEPTH: u32 = 3;

/// What the ledger issues her: `outputs` of `keyset`, which she derived
/// with `ephemeral`, by the hub's own lock under the hash of `preimage`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FundedIssue {
    pub keyset: [u8; 32],
    pub ephemeral: Point,
    pub outputs: Vec<Output>,
    pub preimage: [u8; 32],
}

/// Issue `FundedIssue` on the ledger, settled: the id of its lock. Of a
/// lock under its hash already, that one, settled; else none unless the
/// second argument says to lock anew.
pub type Issuer = dyn Fn(&FundedIssue, bool) -> anyhow::Result<Option<[u8; 32]>> + Send + Sync;

impl Openings {
    /// Issue her funded notes through `f` from now on: without, none are.
    pub fn issue_with(&self, f: Arc<Issuer>) {
        let _ = self.issuer.set(f);
    }

    /// Her funded notes of contract `id`, issued: once issued, always;
    /// else only of an F the hub signed, buried [`FUNDED_DEPTH`] deep at
    /// least, and unspent, even in the mempool. Not a guard against
    /// paying twice: a spend of F meanwhile may follow, and a claim naming
    /// the notes issued retires them (`Hub::retire_claimed`) whether or
    /// not they were, so they are never spent as well.
    pub(super) async fn funded(&self, id: [u8; 32]) -> Result<FundedIssued, Refused> {
        let issuer = self.issuer.get().cloned();
        let issuer =
            issuer.ok_or_else(|| Refused::unavailable("the hub issues no funded notes"))?;
        let ctx = self.context().await?;
        let row = self.store().row(&id).map_err(Refused::unavailable)?;
        let row = row.ok_or_else(|| Refused::bad("no such contract"))?;
        if row.abandoned {
            return Err(Refused::bad("an opening given up"));
        }
        let hub = Hub::load(&row.state, &ctx).map_err(Refused::unavailable)?;
        let notes = hub
            .funded_notes()
            .ok_or_else(|| Refused::bad("she funded none of it"))?;
        let base = hub.base().ok_or_else(|| Refused::bad("no terms"))?;
        let parts = base.parts();
        let issue = FundedIssue {
            keyset: *parts.keyset.id(),
            ephemeral: notes.ephemeral,
            outputs: notes.outputs.clone(),
            preimage: hub.funded_preimage(),
        };
        let preimage = issue.preimage;
        let answer = |lock| FundedIssued { lock, preimage };
        let ledger = issuer.clone();
        let asked = issue.clone();
        let issued = tokio::task::spawn_blocking(move || ledger(&asked, false));
        let issued = issued.await.map_err(Refused::unavailable)?;
        if let Some(lock) = issued.map_err(Refused::unavailable)? {
            return Ok(answer(lock));
        }
        let funding = match hub.phase() {
            HubPhase::Signed | HubPhase::Funded => hub.funding_tx(),
            _ => None,
        };
        let funding = funding.ok_or_else(|| Refused::stale("F is not signed yet"))?;
        let contract = OutPoint {
            txid: funding.compute_txid(),
            vout: 0,
        };
        let depth = parts.chain.funding_depth.max(FUNDED_DEPTH);
        let deep = self.confirmations(contract).await;
        let deep = deep.map_err(Refused::unavailable)?;
        match deep {
            None => return Err(Refused::stale("F is not in a block, or spent")),
            Some(n) if n < depth => {
                return Err(Refused::stale(format!(
                    "F is {n} blocks deep, not yet {depth}"
                )))
            }
            Some(_) => {}
        }
        let issued = tokio::task::spawn_blocking(move || issuer(&issue, true));
        let issued = issued.await.map_err(Refused::unavailable)?;
        let lock = issued.map_err(Refused::unavailable)?;
        let lock = lock.ok_or_else(|| Refused::unavailable("the ledger issued nothing"))?;
        tracing::info!(id = %crate::hex_id(&id), lock = %hex::encode(lock), "her funded notes issued");
        Ok(answer(lock))
    }

    /// How deep `coin` is, unspent even in the mempool: none if spent, or
    /// in no block yet.
    async fn confirmations(&self, coin: OutPoint) -> anyhow::Result<Option<u32>> {
        let out: Option<serde_json::Value> = self
            .wallet
            .rpc_optional(
                "gettxout",
                serde_json::json!([coin.txid.to_string(), coin.vout, true]),
            )
            .await?;
        let n = out.and_then(|o| o.get("confirmations")?.as_u64());
        Ok(n.filter(|n| *n > 0)
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX)))
    }
}
