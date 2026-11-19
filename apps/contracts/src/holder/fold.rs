//! Closing a contract early with its hub, cooperatively, and being paid
//! her claim in the same transaction: its fold, the contract output spent
//! on its key path by both, paying her what her notes at the hub come to,
//! up to its cap, and her reserve back, to her wallet, and the rest to the
//! hub, the fee from the hub's share ([`crate::FOLD_CLAIM`]).
//!
//! Her claim is the one a forced close would file, of settled notes, and
//! it is pinned as her recovery ([`Stage::Folding`](super::recovery::Stage))
//! before anything leaves her: her notes are taken from her balance and
//! the claim's filing kept, but no hold authorization is sent to the hub
//! until she signs. First she asks whether the hub would retire the notes
//! (her nonce, the amount, the notes' identifiers); its offer taken, her
//! close is made past giving up and the fold's txid is kept, then her
//! partial signature and the filing leave together. From then on the hub
//! may sign and broadcast the fold whatever it answers, so the contract
//! counts as closed for good. A fold that did not happen is asked again
//! ([`close`] once more, a fresh nonce, the same claim), or the contract
//! forced closed on that claim, which her recovery does by itself where the
//! hub is silent past the block a forced close is planned for: any fold she
//! signed may confirm still, and pays the same.
//!
//! A contract holding no notes of hers has none to claim: its fold pays her
//! reserve back, as it always did.

use anyhow::{anyhow, ensure, Context as _, Result};
use bitcoin::consensus::deserialize;
use bitcoin::secp256k1::{schnorr, Message, Secp256k1};
use bitcoin::{Amount, Transaction, Txid};
use licash_contract::keys::HolderKeys;
use licash_contract::opening::{Context, Holder};
use licash_wallet::{Transport, Wallet};
use lichen_btc::musig::PubNonce;

use super::driver::failed;
use super::recovery::{self, Stage};
use super::store::Store;
use super::{activation, Chain, Fate};
use crate::{FoldAsk, FoldOffer, FOLD, FOLD_CLAIM, FOLD_SIGN};

/// Close contract `id` early with its hub, in `dir`, its fold paying her
/// notes there (up to its cap) as well as her reserve: its fold's txid,
/// broadcast by the hub and by her. Asked again of one closed already,
/// another fold paying her the same. Refused once forced closed, before F
/// is buried, from its close on, when the default pays her reserve, and
/// near the end of its protection, when only a forced close leaves time to
/// file her claim should the hub not fold; and where locks owed to her
/// depend on it, which a forced close claims.
pub async fn close<T: Transport, C: Chain>(
    wallet: &Wallet<T>,
    chain: &C,
    dir: &std::path::Path,
    id: &[u8; 32],
) -> Result<Txid> {
    let store = Store::existing(dir)?.context("no contracts")?;
    let contract = store
        .list()?
        .into_iter()
        .find(|c| &c.id == id)
        .context("no such contract")?;
    let resuming = match store.recovery(id)? {
        Some(row) => {
            ensure!(
                row.stage == Stage::Folding.as_str(),
                "forced closed already: its recovery goes on"
            );
            // One signed before, asked again; else one that never signed,
            // which [`recovery::fold_claim`] gives back and starts anew.
            row.irrevocable
        }
        None => false,
    };
    let tip = chain.height().await.map_err(|e| anyhow!(e))?;
    ensure!(
        tip < contract.close_height,
        "past its close: the hub's default pays her reserve"
    );
    if !resuming {
        let stake = activation::at_stake(wallet, &store, dir, tip)?;
        ensure!(
            stake.get(id).copied().unwrap_or(0) == 0,
            "locks owed to her depend on this contract, which only a forced close claims: force it closed"
        );
    }
    let (state, since) = store.state(id)?;
    let secrets = wallet.contract_secrets();
    let context = Context {
        tip,
        trust: crate::trust(wallet.network())?,
    };
    let holder = Holder::load(&state, &secrets.seed, &context)?;
    // As the chain says now, not as last seen: buried since, say.
    let mut fate = contract.fate;
    if fate != Some(Fate::Live) {
        if let Some((now, block)) = super::chain::fate(&holder, chain, since).await? {
            store.settle_fate(id, now, block)?;
            fate = Some(now);
        }
    }
    ensure!(
        fate == Some(Fate::Live),
        "its F is not buried in the chain yet"
    );
    // Time to file her claim should the hub take it and not fold.
    let near_end = !resuming
        && tip.saturating_add(1)
            >= recovery::fold_by(&secrets.seed, wallet.network(), dir, id, tip)?;

    // Her claim: her notes there, taken now or by a close before. None if
    // she holds none: the fold then pays her reserve back, and no more.
    let claim = recovery::fold_claim(wallet, chain, dir, id).await?;
    if claim.as_ref().is_some_and(|c| c.fresh) {
        // Her notes back before she is told why not: nothing is spent.
        let why = if near_end {
            Some("too near the end of its protection to fold, with time left to force it closed on her claim: force it closed")
        } else if contract.folded.is_some() {
            // A fold signed before, of no claim, may still confirm: it pays
            // none, and would end her contract with her notes retired.
            Some("an earlier fold of this contract, which pays no claim, may still confirm: wait for it, or force the contract closed")
        } else {
            None
        };
        if let Some(why) = why {
            recovery::give_up(wallet, dir, id)?;
            anyhow::bail!(why);
        }
    }

    let o = holder.signed()?;
    let key = HolderKeys::derive(&secrets.seed, &o.base.parts().nonce)
        .party
        .payment;
    let hub = contract.hub.trim_end_matches('/');
    let url = |route: &str| format!("{hub}{}", route.replace("{id}", &crate::hex_id(id)));
    let paid = claim.as_ref().map_or(Amount::ZERO, |c| {
        Amount::from_sat(u64::from(c.amount) * o.base.parts().graph.unit_sats)
    });

    // Her nonce, of the key both sign with: neither the fee nor the claim
    // changes it. Nothing irrevocable happens before the hub has offered.
    let (sec, mine) = crate::fold(&o.babe, &o.funding, paid, Amount::ZERO)?.nonce(&key, id)?;
    let prepared = async {
        let offer = match &claim {
            None => post(wallet, hub, &url(FOLD), mine.0.to_vec()).await?,
            Some(c) => {
                let ask = FoldAsk {
                    nonce: mine.0,
                    amount: c.amount,
                    notes: c.notes.clone(),
                };
                post(wallet, hub, &url(FOLD_CLAIM), ask.encode()).await?
            }
        };
        let offer = FoldOffer::decode(&offer).context("the hub's fold offer")?;
        // Her output is her claim and her reserve, to her wallet, whatever
        // the fee: the hub pays it from its own. At 1 sat/vB at least, for
        // a node to relay it.
        let fold = crate::fold(&o.babe, &o.funding, paid, Amount::from_sat(offer.fee))?;
        let mut signed_size = fold.tx.clone();
        signed_size.input[0].witness = bitcoin::Witness::from_slice(&[[0u8; 64]]);
        ensure!(
            offer.fee >= signed_size.vsize() as u64,
            "the hub's fee for the fold, {} sat, is under 1 sat/vB",
            offer.fee
        );
        let nonces = [mine, PubNonce::from_slice(&offer.nonce)?];
        let partial = fold.sign(&key, sec, &nonces)?;
        Ok::<_, anyhow::Error>((offer, fold, partial))
    }
    .await;
    let (offer, fold, partial) = match prepared {
        Ok(prepared) => prepared,
        Err(e) => {
            // Not signed: nothing of hers is spent, and her notes come back.
            if claim.as_ref().is_some_and(|c| c.fresh) {
                let _ = recovery::give_up(wallet, dir, id);
            }
            return Err(e);
        }
    };
    let txid = fold.tx.compute_txid();
    match &claim {
        Some(_) => recovery::fold_signing(&store, id, &txid)?,
        None => store.fold(id, &txid)?,
    }
    // Whatever the hub answers, it may have signed: the fold stands.
    let mut body = partial.to_vec();
    if let Some(c) = &claim {
        body.extend(c.filing.to_bytes());
    }
    let answer = post(wallet, hub, &url(FOLD_SIGN), body).await;
    let answer = answer.context("the fold may be signed: it counts as closed, ask again")?;
    let signed: Transaction = deserialize(&answer).context("the hub's fold")?;
    ensure!(
        signed.compute_txid() == txid,
        "the hub answered another transaction"
    );
    let signature = signed.input[0].witness.nth(0).context("an unsigned fold")?;
    let signature = schnorr::Signature::from_slice(signature).context("the fold's signature")?;
    Secp256k1::verification_only()
        .verify_schnorr(
            &signature,
            &Message::from_digest(fold.sighash()?),
            &fold.key.output_key(),
        )
        .context("the fold's signature")?;
    if claim.is_some() {
        if let Err(e) = recovery::fold_signed(&store, id, &signed) {
            tracing::warn!(%txid, "the signed fold not kept: {e:#}");
        }
    }
    if let Err(e) = chain.broadcast(&signed).await {
        tracing::warn!(%txid, "broadcasting the fold: {e}");
    }
    tracing::info!(
        id = %crate::hex_id(id), %txid, fee = offer.fee,
        claimed = claim.as_ref().map(|c| c.amount),
        "contract folded"
    );
    Ok(txid)
}

/// POST `body` to `url` at `hub`: its answer.
async fn post<T: Transport>(
    wallet: &Wallet<T>,
    hub: &str,
    url: &str,
    body: Vec<u8>,
) -> Result<Vec<u8>> {
    let response = wallet.transport().request(url, Some(body)).await;
    let response = response.map_err(|e| anyhow!("{hub}: {e}"))?;
    match response.status {
        200 => Ok(response.body),
        status => Err(failed(hub, status, &response.body)),
    }
}
