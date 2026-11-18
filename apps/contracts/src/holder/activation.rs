//! The locks owed to her on Bitcoin ([`licash_wallet::OwedLock`]): their
//! activations, made as one nears its expiry unpaid ([`keep`]), and as a
//! recovery files them ([`shown`]).
//!
//! An activation reveals her preimage to all: made only while it can
//! confirm before the lock's expiry, which makes its hub owe her the lock,
//! and only if a contract of hers at its hub can claim it then.
//!
//! A claim advertising an activation its statement would not verify is
//! owed nothing at all, her notes with it, so each is checked here first,
//! by the statement's own rule, and one in doubt is not filed: its lock is
//! left to its hub.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use anyhow::{anyhow, ensure, Context as _, Result};
use bitcoin::hashes::Hash as _;
use bitcoin::{FeeRate, ScriptBuf};
use licash::base::{Base, Version};
use licash::chain::{activation_script, Activation, ActivationRef, Chain as Headers, Inclusion};
use licash::recovery::SourceWitness;
use licash::spend::ProposalId;
use licash_contract::opening::{Context, Holder};
use licash_wallet::{LockClaim, OwedLock, Transport, Wallet};

use super::exit::{self, blocks_a_day};
use super::recovery::confirmed_at;
use super::store::Store;
use super::Chain;

/// The most bytes of a block whose transaction's inclusion is captured.
const MAX_BLOCK_BYTES: usize = 4_000_000;

/// The virtual size of an activation, about: one key-path input, its
/// record and change.
const ACTIVATION_VB: u64 = 160;

/// Blocks before a lock's expiry its activation is made from, its hub not
/// having paid it: about two hours, for it to confirm in at the rate of a
/// forced close, rebroadcast each round.
pub fn lead(network: licash::spend::ChainProfile) -> u32 {
    (blocks_a_day(network) / 12).max(6)
}

/// What a round did of an owed lock ([`keep`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kept {
    /// The lock's id, in hex.
    pub id: String,
    /// "paid" by its hub at last, "activated" (broadcast), "confirmed" (its
    /// activation recorded), "forgotten" (its block left the chain), "late"
    /// (no activation can confirm before its expiry), or "unprotected" (no
    /// contract of hers at its hub could claim it).
    pub step: &'static str,
    pub error: Option<String>,
}

/// Take each lock owed to the wallet ([`OwedLock`]) a step on, at the
/// tip of `chain`: as it nears its expiry unpaid, ask its hub once more,
/// then activate it on Bitcoin with a coin of hers free of contracts in
/// `dir`, at `rate`; record its activation once confirmed, and forget it
/// should its block leave the chain.
pub async fn keep<T: Transport, C: Chain>(
    wallet: &Wallet<T>,
    chain: &C,
    dir: &Path,
    rate: FeeRate,
) -> Result<Vec<Kept>> {
    let Some(store) = Store::existing(dir)? else {
        return Ok(vec![]);
    };
    let owed: Vec<OwedLock> = (wallet.owed_locks()?.into_iter())
        .filter(|o| !o.claimed)
        .collect();
    if owed.is_empty() {
        return Ok(vec![]);
    }
    let tip = chain.height().await.map_err(|e| anyhow!(e))?;
    let mut kept = Vec::new();
    for o in &owed {
        let id = hex::encode(o.id.as_bytes());
        match step(wallet, chain, &store, dir, o, tip, rate).await {
            Ok(Some(step)) => {
                tracing::info!(lock = %id, step, "a lock owed to her");
                kept.push(Kept {
                    id,
                    step,
                    error: None,
                });
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(lock = %id, "a lock owed to her: {e:#}");
                kept.push(Kept {
                    id,
                    step: "error",
                    error: Some(format!("{e:#}")),
                });
            }
        }
    }
    Ok(kept)
}

async fn step<T: Transport, C: Chain>(
    wallet: &Wallet<T>,
    chain: &C,
    store: &Store,
    dir: &Path,
    o: &OwedLock,
    tip: u32,
    rate: FeeRate,
) -> Result<Option<&'static str>> {
    if let Some(at) = o.activated_at {
        let claim = wallet.owed_claim(&o.id, &o.keyset)?.context("not owed")?;
        let (record, _) = claim.activation.as_ref().context("no activation")?;
        let hash = chain.hash(at).await.map_err(|e| anyhow!(e))?;
        if hash.to_byte_array() != record.reference.block_hash {
            wallet.record_activation(&o.id, None)?;
            return Ok(Some("forgotten"));
        }
        return Ok(None);
    }
    // Its checkpoint unknown: no activation of it is shown anchored.
    let Some(anchor) = o.anchor.clone() else {
        return Ok(None);
    };
    let script = ScriptBuf::from_bytes(activation_script(&o.preimage));
    let lead = lead(wallet.network());
    let last = o.expiry.saturating_sub(1);
    if let Some(spend) = wallet.silent_spends_paying(&script)?.into_iter().next() {
        let tx = spend.tx;
        let txid = tx.compute_txid();
        // Its record is in no block filter: its change is, if any.
        let found = match tx.output.iter().find(|o| !o.script_pubkey.is_op_return()) {
            Some(change) => {
                let from = o.expiry.saturating_sub(lead + 1).max(anchor.height() + 1);
                confirmed_at(chain, txid, &change.script_pubkey, from..=tip.min(last)).await?
            }
            None => spend.height,
        };
        if let Some(height) = found.filter(|h| *h > anchor.height() && *h <= last) {
            let activation = activation_of(chain, &tx, height).await?;
            wallet.record_activation(&o.id, Some((&activation, &anchor)))?;
            return Ok(Some("confirmed"));
        }
        // Too late to confirm before its expiry: not to reveal it more.
        if tip >= last {
            return Ok(None);
        }
        if let Err(e) = chain.broadcast(&tx).await {
            tracing::warn!(%txid, "rebroadcasting an activation: {e}");
        }
        return Ok(None);
    }
    if tip.saturating_add(lead) < o.expiry {
        return Ok(None);
    }
    if tip >= last {
        return Ok(Some("late"));
    }
    if claimant(wallet, store, dir, o, tip)?.is_none() {
        return Ok(Some("unprotected"));
    }
    // Its hub knows the preimage already: it may pay it still.
    if wallet
        .claim_revealed(&o.hub, &hex::encode(o.id.as_bytes()), &o.preimage)
        .await
        .is_ok()
    {
        return Ok(Some("paid"));
    }
    let fee = (rate.fee_vb(ACTIVATION_VB)).context("an activation's fee overflows")?;
    let coin = (super::coin(wallet, dir, fee.to_sat())?)
        .context("no coin of hers free to activate the lock with")?;
    let tx = wallet.spend_silent_coin(script, coin.outpoint, Some(0), rate)?;
    if let Err(e) = chain.broadcast(&tx).await {
        tracing::warn!(txid = %tx.compute_txid(), "broadcasting an activation: {e}");
    }
    Ok(Some("activated"))
}

/// The activation `tx` makes in block `height` of `chain`, with its
/// inclusion there.
async fn activation_of<C: Chain>(
    chain: &C,
    tx: &bitcoin::Transaction,
    height: u32,
) -> Result<Activation> {
    let hash = chain.hash(height).await.map_err(|e| anyhow!(e))?;
    let block = chain.block(&hash).await.map_err(|e| anyhow!(e))?;
    let txid = tx.compute_txid();
    let index = (block.txdata.iter().position(|t| t.compute_txid() == txid))
        .context("the activation is not in its block")?;
    let output = (tx
        .output
        .iter()
        .position(|o| o.script_pubkey.is_op_return()))
    .context("an activation of no record")?;
    let bytes = bitcoin::consensus::serialize(&block);
    Ok(Activation {
        reference: ActivationRef {
            height,
            block_hash: hash.to_byte_array(),
            txid: txid.to_byte_array(),
            output: u32::try_from(output)?,
        },
        inclusion: Inclusion::capture(&bytes, index, MAX_BLOCK_BYTES)?,
    })
}

/// The contract of hers in `dir` that claims owed lock `o` once
/// activated, at `tip`: of those at its hub protecting notes, not under
/// recovery, of a Base of its keyset valuing a pay branch by its
/// activation, whose checkpoint is no later than its anchor (its claim's
/// chain starts there), and whose forced close her exit plans after its
/// expiry at depth ([`exit::planned_force_at`]), the one planned last.
/// None if none, or its anchor is unknown.
pub(super) fn claimant<T: Transport>(
    wallet: &Wallet<T>,
    store: &Store,
    dir: &Path,
    o: &OwedLock,
    tip: u32,
) -> Result<Option<[u8; 32]>> {
    let Some(anchor) = o.anchor.as_ref() else {
        return Ok(None);
    };
    let urls = wallet.hub_urls(&o.hub)?;
    let recovering: HashSet<[u8; 32]> = store.recoveries()?.iter().map(|r| r.id).collect();
    let after = o.expiry.saturating_add(o.depth);
    let (seed, network) = (wallet.contract_secrets().seed, wallet.network());
    let ctx = Context {
        tip,
        trust: crate::trust(network)?,
    };
    let mut last = None;
    for c in store.list()? {
        if !c.at(&urls) || !c.live(tip) || recovering.contains(&c.id) {
            continue;
        }
        let (state, _) = store.state(&c.id)?;
        let opening = Holder::load(&state, &seed, &ctx)?.signed()?;
        let parts = opening.base.parts();
        if !matches!(opening.base.version(), Version::V4 { .. })
            || parts.chain.checkpoint.height() > anchor.height()
            || *parts.keyset.id() != o.keyset
        {
            continue;
        }
        let force_at = exit::planned_force_at(&seed, network, dir, &c.id, tip)?;
        if force_at >= after && last.is_none_or(|(at, _)| force_at > at) {
            last = Some((force_at, c.id));
        }
    }
    Ok(last.map(|(_, id)| id))
}

/// What the locks owed to her that only a forced close may claim are
/// worth, by the contract claiming each ([`claimant`]), at `tip`: those
/// unclaimed, activated or still to be before their expiry.
pub(super) fn at_stake<T: Transport>(
    wallet: &Wallet<T>,
    store: &Store,
    dir: &Path,
    tip: u32,
) -> Result<HashMap<[u8; 32], u64>> {
    let mut stake = HashMap::new();
    for o in wallet.owed_locks()? {
        if o.claimed || (o.activated_at.is_none() && tip >= o.expiry) {
            continue;
        }
        let Some(id) = claimant(wallet, store, dir, &o, tip)? else {
            continue;
        };
        let Some(claim) = wallet.owed_claim(&o.id, &o.keyset)? else {
            continue;
        };
        *stake.entry(id).or_insert(0u64) += claim.value;
    }
    Ok(stake)
}

/// The owed locks of `base`'s keyset, activated at depth by `tip`, whose
/// activation the statement of a claim filed under `base` shows on
/// `chain`: none unless `base` is of a version that values a pay branch
/// by its activation.
pub(super) async fn shown<T: Transport, C: Chain>(
    wallet: &Wallet<T>,
    chain: &C,
    base: &Base,
    tip: u32,
) -> Result<BTreeSet<ProposalId>> {
    let mut shown = BTreeSet::new();
    if !matches!(base.version(), Version::V4 { .. }) {
        return Ok(shown);
    }
    let parts = base.parts();
    let keyset = *parts.keyset.id();
    let from = parts.chain.checkpoint.height();
    let max = parts.limits.max_transaction_bytes as usize;
    for owed in wallet.owed_locks()? {
        let due = owed.activated_at.zip(Some(owed.depth));
        if owed.claimed
            || owed.keyset != keyset
            || due.is_none_or(|(at, depth)| at.saturating_add(depth) > tip.saturating_add(1))
        {
            continue;
        }
        let Some(claim) = wallet.owed_claim(&owed.id, &keyset)? else {
            continue;
        };
        match verify(chain, &claim, from, max).await {
            Ok(()) => {
                shown.insert(owed.id);
            }
            Err(e) => tracing::warn!(
                lock = %hex::encode(owed.id.as_bytes()),
                "an activation not filed: {e:#}"
            ),
        }
    }
    Ok(shown)
}

/// Check `claim`'s activation as the statement does
/// ([`Activation::verify`]): its anchor a block of `chain` no earlier than
/// `from`, Base's checkpoint, which the claim's chain starts at; its
/// transaction in a block of `chain` after it and before the lock's
/// expiry, buried at the lock's depth, of no more than `max` bytes.
async fn verify<C: Chain>(chain: &C, claim: &LockClaim, from: u32, max: usize) -> Result<()> {
    let (record, witness) = claim.activation.as_ref().context("no activation")?;
    let anchor = witness.anchor.as_ref().context("no anchor")?;
    let SourceWitness::Certified { proposal, .. } = &claim.witness else {
        return Err(anyhow!("not a lock paying her"));
    };
    let terms = proposal.lock().context("a lock of no terms")?.terms();
    ensure!(
        anchor.height() >= from,
        "its anchor, block {}, precedes the contract's checkpoint, block {from}",
        anchor.height()
    );
    let ours = (chain.checkpoint(anchor.height()).await).map_err(|e| anyhow!(e))?;
    ensure!(
        ours.commitment() == anchor.commitment(),
        "its anchor is not of the chain"
    );
    // Through the block that buries it at its depth.
    let last = (record.reference.height)
        .saturating_add(terms.depth())
        .saturating_sub(1);
    let mut headers = Vec::new();
    for height in anchor.height() + 1..=last {
        let header = chain.header(height).await.map_err(|e| anyhow!(e))?;
        let bytes: [u8; 80] = (bitcoin::consensus::serialize(&header).try_into())
            .map_err(|_| anyhow!("a header of other than 80 bytes"))?;
        headers.push(bytes);
    }
    let count = headers.len();
    let headers = Headers::verify(ours, &anchor.commitment(), headers, count)?;
    Activation {
        reference: record.reference,
        inclusion: witness.inclusion.clone(),
    }
    .verify(&headers, anchor, terms, max)?;
    Ok(())
}
