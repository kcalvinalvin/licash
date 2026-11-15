//! The holder's side of openings, in her wallet: [`open`] a contract at
//! a hub she trusts, [`resume`] those under way and watch their F, and
//! [`list`] them. Her deposit is a coin of the wallet's, signed into F by
//! it; her change returns to its fallback address.

mod chain;
mod driver;
mod store;

use std::path::Path;

use anyhow::{anyhow, ensure, Context as _, Result};
use bitcoin::{OutPoint, TxOut};
use licash_contract::opening::{Holder, Start};
use licash_contract::wire::Wanted;
use licash_wallet::{Transport, Wallet};

pub use chain::{Chain, Node};
use driver::Driver;
pub use licash_contract::opening::{Fate, Phase};
pub use store::Contract;
use store::{New, Store};

use crate::{Offer, QUOTE};

/// Blocks from a recovery's assertion deadline to the close: for the
/// hub's challenge and her answer to it, as the regtest fixture has.
pub const AFTER_ASSERT: u32 = 140;

/// The blocks she has to respond to the hub's request, at least.
pub const RESPONSE_WINDOW: u16 = 6;

/// What she asks of a hub.
#[derive(Clone, Copy, Debug)]
pub struct Ask {
    /// Blocks from now to the close, when the hub's collateral returns
    /// to it: at most what the hub offers.
    pub blocks: u32,
    /// The most she pays to open, in sat.
    pub max_price: u64,
}

/// Open a contract at `hub`, a hub `wallet` trusts, on its offer now:
/// its keyset one the hub shows the wallet, its price within `ask`, her
/// deposit the smallest coin free that pays it. Kept in `dir`, the
/// wallet's directory, from her first message on.
pub async fn open<T: Transport, C: Chain>(
    wallet: &Wallet<T>,
    chain: &C,
    dir: &Path,
    hub: &str,
    ask: Ask,
) -> Result<Contract> {
    let hub = hub.trim_end_matches('/').to_owned();
    let trust = crate::trust(wallet.network())?;
    let trusted = wallet.hubs()?;
    let (_, info) = trusted
        .iter()
        .find(|(url, _)| *url == hub)
        .with_context(|| format!("{hub} is no hub this wallet trusts"))?;
    let offer = offer(wallet, &hub).await?;
    let keysets = info
        .keysets()
        .map_err(|e| anyhow!("{hub}'s keysets: {e:?}"))?;
    ensure!(
        keysets.contains(&offer.quote.keyset),
        "{hub} quotes on a keyset it does not show its wallets"
    );
    let price = offer.price();
    ensure!(
        price <= ask.max_price,
        "{hub} asks {price} sat to open, more than {}",
        ask.max_price
    );
    let tip = chain.height().await.map_err(|e| anyhow!(e))?;
    let at = offer.quote.chain.checkpoint.height();
    ensure!(
        at <= tip,
        "{hub} is at block {at}, the chain at {tip}: try again"
    );
    // Hers: the hub's quote is checked against it.
    let checkpoint = chain.checkpoint(at).await.map_err(|e| anyhow!(e))?;
    let wanted = wanted(&offer, tip, ask.blocks)?;
    let store = Store::open(dir)?;
    let (deposit, coin) = deposit(wallet, &store, price)?;
    let secrets = wallet.contract_secrets();
    let start = Start {
        seed: secrets.seed,
        spend: secrets.spend,
        quote: offer.quote.clone(),
        deposit,
        coin: coin.clone(),
        change: wallet.fallback_address()?.script_pubkey(),
        wanted,
        checkpoint,
    };
    let ctx = licash_contract::opening::Context { tip, trust };
    let (holder, outs) = Holder::new(start, &ctx)?;
    let id = holder.id();
    store.insert(&New {
        id,
        hub: &hub,
        mock: trust.mock,
        deposit,
        value: coin.value.to_sat(),
        price,
        cap: offer.quote.cap,
        assert_height: wanted.assert_height,
        close_height: wanted.close_height,
        since: tip,
        state: &holder.to_bytes(),
    })?;
    tracing::info!(id = %crate::hex_id(&id), %hub, %deposit, price, mock = trust.mock, "opening a contract");
    let mut driver = Driver {
        wallet,
        chain,
        store: &store,
        trust,
        hub,
        id,
        holder: Some(holder),
        quote: Some(offer.quote),
    };
    let driven = driver.drive(outs).await;
    driver.settle(driven)?;
    refresh(&driver, chain).await?;
    found(&store, &id)
}

/// Carry on with the openings in `dir` under way, and see where the F
/// of each signed stands: every contract, the newest first. Nothing is
/// made in `dir` if none was ever opened there.
pub async fn resume<T: Transport, C: Chain>(
    wallet: &Wallet<T>,
    chain: &C,
    dir: &Path,
) -> Result<Vec<Contract>> {
    let Some(store) = Store::existing(dir)? else {
        return Ok(Vec::new());
    };
    let tip = chain.height().await.map_err(|e| anyhow!(e))?;
    let trust = crate::trust(wallet.network())?;
    for contract in store.list()? {
        let settled = matches!(contract.fate, Some(Fate::Live | Fate::Dead));
        if contract.over(tip) || settled {
            continue;
        }
        let id = contract.id;
        let (state, _) = store.state(&id)?;
        let secrets = wallet.contract_secrets();
        let ctx = licash_contract::opening::Context { tip, trust };
        let holder = Holder::load(&state, secrets.seed, secrets.spend, &ctx)?;
        let mut driver = Driver {
            wallet,
            chain,
            store: &store,
            trust,
            hub: contract.hub.clone(),
            id,
            holder: Some(holder),
            quote: None,
        };
        let resumed = driver.resume().await;
        if let Err(e) = driver.settle(resumed) {
            tracing::warn!(id = %crate::hex_id(&id), "resuming an opening: {e:#}");
            continue;
        }
        if let Err(e) = refresh(&driver, chain).await {
            tracing::warn!(id = %crate::hex_id(&id), "watching F: {e:#}");
        }
    }
    store.list()
}

/// The contracts in `dir`, the newest first: none if none was ever
/// opened there.
pub fn list(dir: &Path) -> Result<Vec<Contract>> {
    match Store::existing(dir)? {
        Some(store) => store.list(),
        None => Ok(Vec::new()),
    }
}

/// The offer of `hub` now.
pub async fn offer<T: Transport>(wallet: &Wallet<T>, hub: &str) -> Result<Offer> {
    let url = format!("{}{QUOTE}", hub.trim_end_matches('/'));
    let response = wallet.transport().request(&url, None).await;
    let response = response.map_err(|e| anyhow!("{hub}: {e}"))?;
    ensure!(
        response.status == 200,
        "{hub} offers no contracts: status {}",
        response.status
    );
    Offer::decode(&response.body).with_context(|| format!("{hub}'s offer"))
}

/// Her deadlines on `offer` at `tip`: the close `blocks` ahead, no later
/// than the hub's bounds allow, the assertion deadline before it.
fn wanted(offer: &Offer, tip: u32, blocks: u32) -> Result<Wanted> {
    let bounds = &offer.bounds;
    ensure!(
        blocks <= bounds.lock,
        "{blocks} blocks, more than the hub's {}",
        bounds.lock
    );
    // The hub counts from its tip, at its checkpoint or past it.
    let latest = offer.quote.chain.checkpoint.height() + bounds.lock;
    let close_height = (tip + blocks).min(latest);
    let assert_height = close_height
        .checked_sub(AFTER_ASSERT)
        .filter(|a| *a > tip)
        .with_context(|| format!("{blocks} blocks leave no time to recover in"))?;
    Ok(Wanted {
        assert_height,
        close_height,
        response_window: RESPONSE_WINDOW.max(bounds.response_window),
    })
}

/// The smallest coin of `wallet` that pays `price`, free and no other
/// contract's deposit.
fn deposit<T: Transport>(
    wallet: &Wallet<T>,
    store: &Store,
    price: u64,
) -> Result<(OutPoint, TxOut)> {
    let taken: Vec<OutPoint> = store
        .list()?
        .into_iter()
        .filter(|c| c.abandoned.is_none() && c.fate != Some(Fate::Dead))
        .map(|c| c.deposit)
        .collect();
    let mut coins = wallet.silent_coins()?;
    coins.retain(|c| c.spending.is_none() && c.value >= price && !taken.contains(&c.outpoint));
    coins.sort_by_key(|c| c.value);
    let coin = coins
        .first()
        .with_context(|| format!("no coin on Bitcoin of {price} sat at least, free, to deposit"))?;
    let output = wallet
        .silent_coin_output(&coin.outpoint)?
        .context("a coin gone")?;
    Ok((coin.outpoint, output))
}

/// Watch the F of the opening `driver` drives, once she signed it: its
/// fate stored, her coin freed if F can never confirm, or once it lapsed
/// unconfirmed.
async fn refresh<T: Transport, C: Chain>(driver: &Driver<'_, T, C>, chain: &C) -> Result<()> {
    let holder = driver.holder();
    if holder.phase() != Phase::Funded {
        return Ok(());
    }
    let (_, since) = driver.store.state(&driver.id)?;
    let Some(fate) = chain::fate(holder, chain, since).await? else {
        return Ok(());
    };
    driver.store.set_fate(&driver.id, fate)?;
    if fate == Fate::Dead {
        driver.store.forget(&driver.id)?;
    }
    let tip = chain.height().await.map_err(|e| anyhow!(e))?;
    let free = fate == Fate::Dead || (fate == Fate::Open && holder.lapsed(tip));
    if let (true, Some(f)) = (free, holder.funding()) {
        let txid = f.tx.compute_txid();
        // Freed already, or spent: nothing to free.
        let _ = driver.wallet.abandon_silent(&txid);
        tracing::warn!(id = %crate::hex_id(&driver.id), %txid, ?fate, "F will not confirm: her coin freed");
    }
    Ok(())
}

fn found(store: &Store, id: &[u8; 32]) -> Result<Contract> {
    store
        .list()?
        .into_iter()
        .find(|c| c.id == *id)
        .context("the contract just stored")
}
