//! Closing a contract early with its hub: its fold, the contract output
//! spent on its key path by both, paying her reserve back to her wallet
//! and the rest to the hub, the fee from the hub's share ([`crate::FOLD`]).
//!
//! The fold's txid is kept before her partial signature leaves: from then
//! on the hub may sign and broadcast it whatever it answers, so the
//! contract counts as closed for good. A fold that did not happen is
//! asked again ([`close`] once more, a fresh nonce, the same payout to
//! her), or the contract forced closed: its recovery gives back if the
//! fold confirmed.

use anyhow::{anyhow, ensure, Context as _, Result};
use bitcoin::consensus::deserialize;
use bitcoin::secp256k1::{schnorr, Message, Secp256k1};
use bitcoin::{Amount, Transaction, Txid};
use licash_contract::keys::HolderKeys;
use licash_contract::opening::{Context, Holder};
use licash_wallet::{Transport, Wallet};
use lichen_btc::musig::PubNonce;

use super::driver::failed;
use super::store::Store;
use super::{Chain, Fate};
use crate::{FoldOffer, FOLD, FOLD_SIGN};

/// Close contract `id` early with its hub, in `dir`: its fold's txid,
/// broadcast by the hub and by her. Asked again of one closed already,
/// another fold paying her the same. Refused once forced closed, before F
/// is buried, or from its close on, when the default pays her reserve.
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
    ensure!(
        store.recovery(id)?.is_none(),
        "forced closed already: its recovery goes on"
    );
    let tip = chain.height().await.map_err(|e| anyhow!(e))?;
    ensure!(
        tip < contract.close_height,
        "past its close: the hub's default pays her reserve"
    );
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
    let o = holder.signed()?;
    let key = HolderKeys::derive(&secrets.seed, &o.base.parts().nonce)
        .party
        .payment;
    let hub = contract.hub.trim_end_matches('/');
    let url = |route: &str| format!("{hub}{}", route.replace("{id}", &crate::hex_id(id)));

    // Her nonce, of the key both sign with: the fee does not change it.
    let (sec, mine) = crate::fold(&o.babe, &o.funding, Amount::ZERO)?.nonce(&key, id)?;
    let offer = post(wallet, hub, &url(FOLD), mine.0.to_vec()).await?;
    let offer = FoldOffer::decode(&offer).context("the hub's fold offer")?;
    // Her output is her reserve, to her wallet, whatever the fee: the hub
    // pays it from its own.
    let fold = crate::fold(&o.babe, &o.funding, Amount::from_sat(offer.fee))?;
    let nonces = [mine, PubNonce::from_slice(&offer.nonce)?];
    let partial = fold.sign(&key, sec, &nonces)?;
    let txid = fold.tx.compute_txid();
    store.fold(id, &txid)?;
    // Whatever the hub answers, it may have signed: the fold stands.
    let answer = post(wallet, hub, &url(FOLD_SIGN), partial.to_vec()).await;
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
    if let Err(e) = chain.broadcast(&signed).await {
        tracing::warn!(%txid, "broadcasting the fold: {e}");
    }
    tracing::info!(id = %crate::hex_id(id), %txid, fee = offer.fee, "contract folded");
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
