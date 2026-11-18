//! The notes she funds a contract with herself (a v5 Base): its hub
//! issues them her once F is buried ([`crate::FUNDED`]), by a lock of its
//! own, settled, she claims as any. Until her wallet holds every one,
//! the contract keeps them owed: her recovery claims those it does not
//! hold, and no fold gives them up.

use anyhow::{Context as _, Result};
use licash_contract::opening::{Context, Fate, Holder};
use licash_contract::wire::FundedNotes;
use licash_wallet::{Error as WalletError, FundedNote, Transport, Wallet};

use super::{fold::post, Store};
use crate::{FundedIssued, FUNDED};

/// Ask the hub of each contract in `store` whose funded notes her wallet
/// has not received for them, at `tip`: of one whose F is buried and
/// which she neither forced closed nor folded. Those received are kept
/// as such; a hub not issuing yet is asked again next time.
pub(super) async fn collect<T: Transport>(wallet: &Wallet<T>, store: &Store, tip: u32) {
    let ids = match store.funded_unreceived() {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!("her funded notes: {e:#}");
            return;
        }
    };
    if ids.is_empty() {
        return;
    }
    let contracts = match store.list() {
        Ok(all) => all,
        Err(e) => {
            tracing::warn!("her funded notes: {e:#}");
            return;
        }
    };
    for c in contracts.iter().filter(|c| ids.contains(&c.id)) {
        let forced = store.recovery(&c.id).map(|r| r.is_some());
        if c.fate != Some(Fate::Live) || c.folded.is_some() || !matches!(forced, Ok(false)) {
            continue;
        }
        let id = crate::hex_id(&c.id);
        match collect_one(wallet, store, &c.id, &c.hub, tip).await {
            Ok(true) => tracing::info!(%id, funded = c.funded, "her funded notes received"),
            Ok(false) => {}
            Err(e) => tracing::info!(%id, "her funded notes not received yet: {e:#}"),
        }
    }
}

/// Her funded notes of contract `id` at `hub`, asked and claimed: whether
/// her wallet now holds them all.
async fn collect_one<T: Transport>(
    wallet: &Wallet<T>,
    store: &Store,
    id: &[u8; 32],
    hub: &str,
    tip: u32,
) -> Result<bool> {
    let (state, _) = store.state(id)?;
    let secrets = wallet.contract_secrets();
    let ctx = Context {
        tip,
        trust: crate::trust(wallet.network())?,
    };
    let holder = Holder::load(&state, &secrets.seed, &ctx)?;
    let notes = holder.funded_notes().context("she funded none of it")?;
    if held(wallet, store, id, &holder)? {
        return Ok(true);
    }
    let hub = hub.trim_end_matches('/');
    let url = format!("{hub}{}", FUNDED.replace("{id}", &crate::hex_id(id)));
    let answer = post(wallet, hub, &url, Vec::new()).await?;
    let issued = FundedIssued::decode(&answer).context("the hub's issuance")?;
    let lock = hex::encode(issued.lock);
    match wallet.claim(hub, &lock, &issued.preimage).await {
        Ok(_) | Err(WalletError::AlreadyReceived) => {}
        Err(e) => return Err(e).context("claiming her funded notes"),
    }
    if held(wallet, store, id, &holder)? {
        return Ok(true);
    }
    tracing::warn!(
        id = %crate::hex_id(id),
        %lock,
        outputs = notes.outputs.len(),
        "the hub issued her fewer funded notes than she funded: her recovery claims the rest"
    );
    Ok(false)
}

/// Whether her wallet holds every funded note of contract `id`, which
/// `holder` drives: kept as received if so.
fn held<T: Transport>(
    wallet: &Wallet<T>,
    store: &Store,
    id: &[u8; 32],
    holder: &Holder,
) -> Result<bool> {
    let Some(notes) = holder.funded_notes() else {
        return Ok(true);
    };
    let held = wallet.funded_held(&notes.ephemeral, &notes.outputs)?;
    let all = held.iter().all(|h| *h);
    if all {
        store.funded_received(id)?;
    }
    Ok(all)
}

/// The funded notes of `notes`, a contract's, her wallet does not hold:
/// what her claim names them by. None of one funding none.
pub fn unreceived<T: Transport>(
    wallet: &Wallet<T>,
    notes: Option<&FundedNotes>,
) -> Result<Vec<FundedNote>> {
    let Some(notes) = notes else {
        return Ok(vec![]);
    };
    let held = wallet.funded_held(&notes.ephemeral, &notes.outputs)?;
    let all = wallet.funded_notes(&notes.ephemeral, &notes.outputs)?;
    Ok(all
        .into_iter()
        .zip(held)
        .filter(|(_, h)| !h)
        .map(|(n, _)| n)
        .collect())
}
