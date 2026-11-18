//! The holder's side of openings, in her wallet: [`open`] a contract at
//! a hub she trusts, [`resume`] those under way and watch their F, and
//! [`list`] them. Her deposit is a coin of the wallet's, signed into F by
//! it; her change returns to its fallback address, or into notes at the
//! hub ([`Change::Notes`]). One her hub left silent she forces closed, in
//! [`recovery`]. What a hub charged as a contract opened there is the most
//! it may charge her since, until she agrees to more: [`fees`]. Her
//! contracts at a hub share her notes there out, to show and to claim
//! alike: [`claims`].

mod chain;
pub mod claims;
mod driver;
pub mod exit;
pub mod fees;
mod fold;
#[cfg(feature = "node")]
pub(crate) mod node;
pub mod recovery;
pub mod safety;
mod store;

use std::{collections::HashSet, path::Path};

use anyhow::{anyhow, bail, ensure, Context as _, Result};
use bitcoin::{OutPoint, ScriptBuf};
use licash_contract::opening::{Holder, Quote, Start};
use licash_contract::wire::Wanted;
use licash_wallet::{SilentCoin, Transport, Wallet};

pub use chain::Chain;
use driver::Driver;
pub use driver::{stepping, traffic, Traffic};
pub use fold::close;
pub use licash_contract::opening::{Fate, Phase};
#[cfg(feature = "node")]
pub use node::Node;
pub use store::{kept, Contract};
use store::{New, Store};

use crate::{Offer, QUOTE};

/// Blocks from a recovery's assertion deadline to the close, at least:
/// as the regtest fixture has.
pub const AFTER_ASSERT: u32 = 140;

/// Blocks to spare beyond the windows after an assertion, for what is
/// published in them to confirm.
const SPARE: u32 = 12;

/// Blocks from a recovery's assertion deadline to the close on `quote`:
/// the hub's challenge, her answer to it and the delays on what she is
/// paid after, with `SPARE` blocks to confirm in; [`AFTER_ASSERT`] at
/// least.
pub fn after_assert(quote: &Quote) -> u32 {
    let g = &quote.graph;
    let windows: u32 = [g.challenge_window, g.answer_window, g.delta, g.delta_prime]
        .map(u32::from)
        .iter()
        .sum();
    (windows + SPARE).max(AFTER_ASSERT)
}

/// The blocks she has to respond to the hub's request, at least.
pub const RESPONSE_WINDOW: u16 = 8;

/// Blocks F paying a deposit has, at least, from her signing it to the
/// last its payment may confirm in: twice that when it is asked for, the
/// opening's messages and downloads to come first. Short, not to give an
/// opening up for nothing: one paid too late is taken back after the
/// deposit's timeout.
pub const DEPOSIT_MARGIN: u32 = 24;

/// Where F's change goes, her deposit less her price.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Change {
    /// To the wallet's fallback address, on Bitcoin.
    #[default]
    Bitcoin,
    /// Into notes at the contract's hub, F paying a deposit there with
    /// it, the hub's fee in it: no change left on Bitcoin, her coin
    /// turned whole into the contract and notes it protects. The opening
    /// is refused where that cannot be: the hub takes no deposit paid so
    /// much exactly, leaves too little time to pay it in, or the change
    /// is more than the contract protects. Change too little to be an
    /// output goes to F's fee.
    Notes,
}

/// What she asks of a hub.
#[derive(Clone, Copy, Debug)]
pub struct Ask {
    /// Blocks from now to the close, when the hub's collateral returns
    /// to it: at most what the hub offers.
    pub blocks: u32,
    /// The most she pays to open, in sat.
    pub max_price: u64,
    /// The most notes it protects, in sat, of terms she chooses within
    /// the hub's range (its info's), over `blocks` from the hub's quote:
    /// none for its fixed terms.
    pub cap: Option<u32>,
    /// The coin she deposits: none for the smallest free that pays the
    /// price and is large enough for the cap.
    pub deposit: Option<OutPoint>,
}

/// Open a contract at `hub`, a hub `wallet` trusts, on its offer now:
/// its keyset one the hub shows the wallet, its terms as `ask`ed, its
/// price within it, her deposit the coin she names or else the smallest
/// free that pays it ([`coin`]). Kept in `dir`, the wallet's directory,
/// from her first message on, with the fees the hub charges now: the most
/// it may charge her since ([`fees`]). Refused at a hub charging more
/// than is sane ([`fees::abusive`]).
/// Driven once: an upload dropped leaves it to [`resume`].
pub async fn open<T: Transport, C: Chain>(
    wallet: &Wallet<T>,
    chain: &C,
    dir: &Path,
    hub: &str,
    ask: Ask,
) -> Result<Contract> {
    open_at(wallet, chain, dir, hub, ask, Change::Bitcoin, true, 0).await
}

/// How many times [`open_with`] resumes an opening that failed short of
/// F but for a refusal: over Tor, a 25 MB instance's upload drops now and
/// then, a step's answer too.
const RESUMES: u32 = 2;

/// [`open`], F's `change` going where asked: into notes at the hub, F
/// sweeping her coin whole, if it can. Resumed at once, up to `RESUMES`
/// times, should it fail short of F but for a refusal.
pub async fn open_with<T: Transport, C: Chain>(
    wallet: &Wallet<T>,
    chain: &C,
    dir: &Path,
    hub: &str,
    ask: Ask,
    change: Change,
) -> Result<Contract> {
    open_at(wallet, chain, dir, hub, ask, change, true, RESUMES).await
}

/// [`open_with`], at a hub charging more than is sane too
/// ([`fees::abusive`]): for her who was told, and asks all the same.
pub async fn open_anyway<T: Transport, C: Chain>(
    wallet: &Wallet<T>,
    chain: &C,
    dir: &Path,
    hub: &str,
    ask: Ask,
    change: Change,
) -> Result<Contract> {
    open_at(wallet, chain, dir, hub, ask, change, false, RESUMES).await
}

/// One opening as [`open_once`], or, refused as the hub quotes otherwise
/// now (its fee surcharged since, say), given up and made once more on a
/// quote asked anew: within her ceilings still.
#[allow(clippy::too_many_arguments)]
async fn open_at<T: Transport, C: Chain>(
    wallet: &Wallet<T>,
    chain: &C,
    dir: &Path,
    hub: &str,
    ask: Ask,
    change: Change,
    sane: bool,
    resumes: u32,
) -> Result<Contract> {
    let opened = open_once(wallet, chain, dir, hub, ask, change, sane, resumes).await;
    match opened {
        Err(e) if e.downcast_ref::<driver::Requote>().is_some() => {
            tracing::info!(%hub, "its quote refused as the hub's no longer: asked again: {e:#}");
            open_once(wallet, chain, dir, hub, ask, change, sane, resumes).await
        }
        opened => opened,
    }
}

#[allow(clippy::too_many_arguments)]
async fn open_once<T: Transport, C: Chain>(
    wallet: &Wallet<T>,
    chain: &C,
    dir: &Path,
    hub: &str,
    ask: Ask,
    change: Change,
    sane: bool,
    resumes: u32,
) -> Result<Contract> {
    let hub = hub.trim_end_matches('/').to_owned();
    let trust = crate::trust(wallet.network())?;
    let trusted = wallet.hubs()?;
    let (_, info) = trusted
        .iter()
        .find(|(url, _)| *url == hub)
        .with_context(|| format!("{hub} is no hub this wallet trusts"))?;
    let chosen = ask.cap.map(|cap| (cap, ask.blocks));
    // What it charges now, not as the wallet last heard: her ceilings.
    let now = wallet.hub_info(&hub).await;
    let now = now.map_err(|e| anyhow!("{hub}'s fees: {e}"))?;
    let range = now
        .contracts
        .and_then(|c| c.range)
        .filter(|_| chosen.is_some());
    if let Some(why) = fees::abusive(&now.fees, range.as_ref()).filter(|_| sane) {
        bail!("{hub} charges more than is sane, {why}: no contract opens there unless asked all the same");
    }
    let offer = offer(wallet, &hub, chosen).await?;
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
    let least = least(info.contracts.and_then(|c| c.range), offer.quote.cap, price);
    let free = free(wallet, &store)?;
    let coin = match ask.deposit {
        Some(named) => {
            let coin = free.into_iter().find(|c| c.outpoint == named);
            let coin = coin.with_context(|| format!("{named}: no coin free to deposit"))?;
            ensure!(
                coin.value >= least,
                "{named}: {} sat, not {least} at least",
                coin.value
            );
            coin
        }
        None => smallest(free, least).with_context(|| {
            format!("no coin on Bitcoin of {least} sat at least, free, to deposit")
        })?,
    };
    let deposit = coin.outpoint;
    let coin = wallet
        .silent_coin_output(&deposit)?
        .context("a coin gone")?;
    let left = coin.value.to_sat() - price;
    // A hub whose deposits are paid within fewer blocks would answer
    // one too short to pay in F: not asked.
    let window = info.deposits.as_ref().map_or(0, |d| d.window);
    let into = match change {
        Change::Notes if left >= offer.quote.graph.dust => {
            let cap = u64::from(offer.quote.cap);
            ensure!(
                left <= cap,
                "{deposit}: {left} sat left over the price, more than the {cap} sat protected: \
                 choose a smaller coin or a larger cap"
            );
            ensure!(
                window >= 2 * DEPOSIT_MARGIN,
                "{hub} takes deposits for too short a while to pay {left} sat in F: \
                 choose a coin of the price"
            );
            Some(deposit_for_change(wallet, &hub, left, tip).await?)
        }
        _ => None,
    };
    let secrets = wallet.contract_secrets();
    let start = Start {
        seed: secrets.seed,
        quote: offer.quote.clone(),
        deposit,
        coin: coin.clone(),
        change: match &into {
            Some((_, script)) => script.clone(),
            None => wallet.fallback_address()?.script_pubkey(),
        },
        // Her reserve back at the close, or after a corrected payout: to
        // her wallet, which finds it as any payment.
        payout: wallet.fallback_address()?.script_pubkey(),
        wanted,
        checkpoint,
    };
    let ctx = licash_contract::opening::Context { tip, trust };
    // Its deposit given up should the opening not start.
    let give_up = |why: &str| {
        if let Some((hash, _)) = &into {
            if let Err(e) = wallet.give_up_deposit(hash, why) {
                tracing::warn!(%hash, "giving the deposit of F's change up: {e}");
            }
        }
    };
    let (holder, outs) = match Holder::start(start, &ctx) {
        Ok(started) => started,
        Err(e) => {
            give_up("its opening never started");
            return Err(e);
        }
    };
    let id = holder.id();
    let new = New {
        id,
        hub: &hub,
        mock: trust.mock,
        deposit,
        value: coin.value.to_sat(),
        price,
        cap: offer.quote.cap,
        assert_height: wanted.request_height,
        close_height: wanted.close_height,
        since: tip,
        state: &holder.to_bytes(),
    };
    let inserted = store.insert_under(&new, &now.fees);
    if let Err(e) = inserted {
        give_up("its opening never started");
        return Err(e);
    }
    let into_notes = into.as_ref().map(|(hash, _)| hash.as_str());
    tracing::info!(id = %crate::hex_id(&id), %hub, %deposit, price, mock = trust.mock, ?into_notes, "opening a contract");
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
    let driven = driver.carry_on(driven, resumes).await;
    driver.settle(driven)?;
    refresh(&driver, chain).await?;
    found(&store, &id)
}

/// How long an opening may take, from her first message to her signing
/// F: over Tor, minutes, an hour at worst. Its hub gives up one idle for
/// an hour; resumed past this, an upload dropped would be sent again and
/// again, 25 MB at a time.
pub const OPENS_WITHIN: i64 = 86_400;

/// The openings this process found it cannot load: those whose F she
/// signed, kept as they are, not read again at each of the app's syncs
/// for nothing. Not stored: a later build may read them.
fn unreadable() -> std::sync::MutexGuard<'static, std::collections::BTreeSet<[u8; 32]>> {
    static UNREADABLE: std::sync::Mutex<std::collections::BTreeSet<[u8; 32]>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    UNREADABLE.lock().unwrap_or_else(|e| e.into_inner())
}

/// An opening at `phase`, begun at `began`, still short of her signing F
/// [`OPENS_WITHIN`] later, at `now`: to give up. She never signed her
/// deposit into an F, so none can confirm, and her coin is free.
fn overdue(phase: Phase, began: i64, now: i64) -> bool {
    phase < Phase::Signing && now - began > OPENS_WITHIN
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    for contract in store.list()? {
        let id = contract.id;
        if contract.over(tip) || contract.fate == Some(Fate::Dead) {
            continue;
        }
        // Live: carried on, F broadcast again and its fate found anew, only
        // once its block left the chain.
        if contract.fate == Some(Fate::Live) {
            match unburied_f(&store, chain, &id, tip).await {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    tracing::warn!(id = %crate::hex_id(&id), "following F's block: {e:#}");
                    continue;
                }
            }
        }
        if unreadable().contains(&id) {
            continue;
        }
        if overdue(contract.phase, contract.created_at, now) {
            tracing::warn!(id = %crate::hex_id(&id), "an opening not opened in a day: given up");
            store.abandon(&id, "not opened in a day")?;
            continue;
        }
        let (state, _) = store.state(&id)?;
        let secrets = wallet.contract_secrets();
        let ctx = licash_contract::opening::Context { tip, trust };
        let holder = match Holder::load(&state, &secrets.seed, &ctx) {
            Ok(holder) => holder,
            Err(e) => {
                // One that cannot be read (opened under programs this build
                // does not name, say) holds up none of the others: given up
                // if she never signed its F, else kept as it is.
                tracing::warn!(id = %crate::hex_id(&id), "loading an opening: {e:#}");
                store.abandon(&id, &format!("cannot be carried on: {e}"))?;
                unreadable().insert(id);
                continue;
            }
        };
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
    let all = store.list()?;
    drop(store);
    // As they stand now: one newly buried, or past its time.
    if let Err(e) = fees::agree(wallet, dir, Some(tip)) {
        tracing::warn!("what her contracts' hubs may charge: {e:#}");
    }
    Ok(all)
}

/// Whether F of the live contract `id` left the chain, at `tip`, since
/// found buried: the block it was found in, followed until
/// [`store::BURIED`] deep, replaced. Its fate is then open again, to be
/// found anew. One found live by a build before is not followed: trusted
/// as it stands.
async fn unburied_f(store: &Store, chain: &impl Chain, id: &[u8; 32], tip: u32) -> Result<bool> {
    let Some((height, hash)) = store.fate_block(id)? else {
        return Ok(false);
    };
    match chain.hash(height).await {
        Ok(now) if now == hash => {
            if tip.saturating_add(1).saturating_sub(height) >= store::BURIED {
                store.buried(id)?;
            }
            Ok(false)
        }
        Ok(_) => {
            tracing::warn!(id = %crate::hex_id(id), height, "F's block left the chain: its fate found again");
            store.unbury(id)?;
            Ok(true)
        }
        // A chain shorter, or not read: looked at again next time.
        Err(e) => {
            tracing::debug!(id = %crate::hex_id(id), "no block at {height}: {e}");
            Ok(false)
        }
    }
}

/// The contracts in `dir`, the newest first: none if none was ever
/// opened there.
pub fn list(dir: &Path) -> Result<Vec<Contract>> {
    match Store::existing(dir)? {
        Some(store) => store.list(),
        None => Ok(Vec::new()),
    }
}

/// Snapshot the contract database while the caller holds its driver lock.
/// Instance files are copied under the same lock by the bundle writer.
pub fn snapshot_database(dir: &Path, to: &Path) -> Result<bool> {
    let Some(store) = Store::existing(dir)? else {
        return Ok(false);
    };
    store.snapshot(to)?;
    Ok(true)
}

/// Check a restored store before installing it, without advancing a driver
/// or touching the chain: every recovery row must be one this build carries
/// on, and every signed opening it can read must have the instances its
/// recovery needs. An opening this build cannot read is carried as it was,
/// as on the phone it came from; it covers nothing here either.
pub fn validate_backup<T: Transport>(wallet: &Wallet<T>, dir: &Path) -> Result<()> {
    let Some(store) = Store::existing(dir)? else {
        return Ok(());
    };
    recovery::validate_backup(&store).context("this backup needs a newer version of the app")?;
    let ctx = licash_contract::opening::Context {
        tip: 0,
        trust: crate::trust(wallet.network())?,
    };
    let keys = wallet.contract_secrets();
    for c in store.list()? {
        if c.phase < Phase::Funded {
            continue;
        }
        let (state, _) = store.state(&c.id)?;
        if let Err(e) = Holder::load(&state, &keys.seed, &ctx).and_then(|holder| holder.signed()) {
            tracing::warn!(id = %crate::hex_id(&c.id), "restoring an unreadable opening as is: {e:#}");
        }
    }
    Ok(())
}

/// Financial and technical details from the holder's kept signed opening.
pub struct Inspection {
    pub address: String,
    pub locked: u64,
    pub payout_pool: u64,
    pub holder_reserve: u64,
    pub hub_reserve: u64,
    pub claim_limit: u32,
    pub note_limit: u32,
    pub denominations: Vec<u64>,
    pub keyset: String,
    pub graph_version: u32,
    pub program: String,
}

/// Read only when requested. Earlier opening stages may not have a signed
/// graph yet, in which case these details are unavailable rather than guessed.
pub fn inspect<T: Transport>(
    wallet: &Wallet<T>,
    dir: &Path,
    id: &[u8; 32],
    tip: u32,
) -> Result<Inspection> {
    let store = Store::existing(dir)?.context("no contracts")?;
    let (state, _) = store.state(id)?;
    let secrets = wallet.contract_secrets();
    let ctx = licash_contract::opening::Context {
        tip,
        trust: crate::trust(wallet.network())?,
    };
    let opening = Holder::load(&state, &secrets.seed, &ctx)?.signed()?;
    let network = match wallet.network() {
        licash::spend::ChainProfile::Core30Mainnet => bitcoin::Network::Bitcoin,
        licash::spend::ChainProfile::Core30DefaultRegtest => bitcoin::Network::Regtest,
        _ => bitcoin::Network::Signet,
    };
    inspection(&opening, network)
}

fn inspection(
    opening: &licash_contract::opening::Opening,
    network: bitcoin::Network,
) -> Result<Inspection> {
    let parts = opening.base.parts();
    let script = opening.babe.tree()?.script_pubkey();
    let locked = opening
        .funding
        .output
        .iter()
        .find(|out| out.script_pubkey == script)
        .context("no contract output")?
        .value
        .to_sat();
    ensure!(
        locked == parts.graph.graph_value()?,
        "contract lock disagrees with its signed terms"
    );
    Ok(Inspection {
        address: bitcoin::Address::from_script(&script, network)?.to_string(),
        locked,
        payout_pool: u64::from(parts.graph.maximum_amount()?),
        holder_reserve: parts.graph.reserve_alice,
        hub_reserve: parts.graph.reserve_hub,
        claim_limit: parts.cap,
        note_limit: parts
            .limits
            .max_authorizations
            .min(licash::claim::MAX_ITEMS as u32),
        denominations: parts.keyset.denominations().collect(),
        keyset: crate::hex_id(parts.keyset.id()),
        graph_version: parts.recipe.graph_version,
        program: crate::hex_id(&parts.recipe.program),
    })
}

/// Precisely what live contracts can claim now, by id: what their
/// recoveries would claim of the notes at their hubs, shared out among
/// them ([`claims`]), from the terms they opened with rather than current
/// hub offers. One under recovery shows none: its recovery fixes its notes
/// once, and it cannot also protect those left over or received after.
/// It still takes its share, which the others then cannot claim. One whose
/// opening cannot be read or is of an untrusted holder program covers
/// nothing; this never triggers recovery or reserves notes.
pub fn coverage<T: Transport>(
    wallet: &Wallet<T>,
    dir: &Path,
    contracts: &[Contract],
    tip: u32,
) -> Result<std::collections::HashMap<[u8; 32], u64>> {
    let Some(store) = Store::existing(dir)?.filter(|_| tip > 0) else {
        return Ok(std::collections::HashMap::new());
    };
    let held = wallet.balance()?;
    let live = coverage_order(contracts, tip, &held, &HashSet::new());
    let terms = claims::terms(wallet, &store, &live, tip)?;
    let mut hubs = Vec::new();
    for (url, _) in wallet.hubs()? {
        hubs.push(wallet.hub_urls(&url)?);
    }
    let mut amounts = claims::shared_out(&live, &terms, &hubs, |slots| {
        Ok::<_, anyhow::Error>(wallet.recoverable(slots)?)
    })?;
    let recovering: HashSet<_> = store.recoveries()?.into_iter().map(|r| r.id).collect();
    amounts.retain(|id, _| !recovering.contains(id));
    Ok(amounts)
}

fn coverage_order<'a>(
    contracts: &'a [Contract],
    tip: u32,
    held: &[licash_wallet::Balance],
    recovering: &HashSet<[u8; 32]>,
) -> Vec<&'a Contract> {
    let mut live: Vec<_> = contracts
        .iter()
        .filter(|c| {
            tip > 0
                && c.live(tip)
                && !recovering.contains(&c.id)
                && held
                    .iter()
                    .any(|at| at.hub.trim_end_matches('/') == c.hub.trim_end_matches('/'))
        })
        .collect();
    live.sort_by_key(|c| (c.assert_height, c.id));
    live
}

/// The output of a contract's F in `dir`, confirmed, paying `pays` sat
/// to `script`: a deposit she had F pay her change into
/// ([`Change::Notes`]), to take back should its notes never come, its
/// hub not having seen it in time. None if no F does. Only the contracts
/// whose change is so much are read whole, at `tip`.
pub fn paid_into<T: Transport>(
    wallet: &Wallet<T>,
    dir: &Path,
    script: &bitcoin::Script,
    pays: u64,
    tip: u32,
) -> Result<Option<OutPoint>> {
    let Some(store) = Store::existing(dir)? else {
        return Ok(None);
    };
    let trust = crate::trust(wallet.network())?;
    for contract in store.list()? {
        let live = contract.fate == Some(Fate::Live);
        if !live || contract.value.checked_sub(contract.price) != Some(pays) {
            continue;
        }
        let (state, _) = store.state(&contract.id)?;
        let secrets = wallet.contract_secrets();
        let ctx = licash_contract::opening::Context { tip, trust };
        let holder = match Holder::load(&state, &secrets.seed, &ctx) {
            Ok(holder) => holder,
            Err(e) => {
                tracing::warn!(id = %crate::hex_id(&contract.id), "loading a contract: {e:#}");
                continue;
            }
        };
        let Some(f) = holder.funding().filter(|_| holder.change() == script) else {
            continue;
        };
        let txid = f.tx.compute_txid();
        let paying =
            f.tx.output
                .iter()
                .position(|o| o.script_pubkey == *script && o.value.to_sat() == pays);
        if let Some(vout) = paying {
            let vout = u32::try_from(vout)?;
            return Ok(Some(OutPoint { txid, vout }));
        }
    }
    Ok(None)
}

/// The coin of `wallet`'s that [`open`] would deposit of `least` sat at
/// least ([`least`]), with the contracts in `dir`: the smallest free.
/// None if no coin does.
pub fn coin<T: Transport>(
    wallet: &Wallet<T>,
    dir: &Path,
    least: u64,
) -> Result<Option<SilentCoin>> {
    Ok(smallest(coins(wallet, dir)?, least))
}

/// The coins of `wallet`'s free to deposit, with the contracts in `dir`:
/// confirmed on Bitcoin, unspent and no other contract's deposit, the
/// largest first.
pub fn coins<T: Transport>(wallet: &Wallet<T>, dir: &Path) -> Result<Vec<SilentCoin>> {
    let mut coins = match Store::existing(dir)? {
        Some(store) => free(wallet, &store)?,
        None => free_of(wallet, &[])?,
    };
    coins.sort_by_key(|c| std::cmp::Reverse(c.value));
    Ok(coins)
}

/// The least a coin she deposits holds: the price, and for a cap she
/// chose within a hub's `range`, the cap over the times her coin it may
/// be ([`licash_api::ContractRange::per_coin`]).
pub fn least(range: Option<licash_api::ContractRange>, cap: u32, price: u64) -> u64 {
    let per_coin = range.map_or(0, |r| u64::from(r.per_coin));
    match per_coin {
        0 => price,
        k => price.max(u64::from(cap).div_ceil(k)),
    }
}

/// The offer of `hub` now: of `chosen` terms, a cap in sat over blocks,
/// if any, else its fixed ones. Refused unless of those terms exactly,
/// as a hub offering no others answers.
pub async fn offer<T: Transport>(
    wallet: &Wallet<T>,
    hub: &str,
    chosen: Option<(u32, u32)>,
) -> Result<Offer> {
    let mut url = format!("{}{QUOTE}", hub.trim_end_matches('/'));
    if let Some((cap, blocks)) = chosen {
        url.push_str(&format!("?cap={cap}&blocks={blocks}"));
    }
    let response = wallet.transport().request(&url, None).await;
    let response = response.map_err(|e| anyhow!("{hub}: {e}"))?;
    ensure!(
        response.status == 200,
        "{hub} offers no contracts{}: status {}",
        match chosen {
            Some((cap, blocks)) => format!(" of {cap} sat over {blocks} blocks"),
            None => String::new(),
        },
        response.status
    );
    let offer = Offer::decode(&response.body).with_context(|| format!("{hub}'s offer"))?;
    if let Some((cap, blocks)) = chosen {
        let (quoted, lock) = (offer.quote.cap, offer.bounds.lock);
        ensure!(
            (quoted, lock) == (cap, blocks),
            "{hub} offers contracts of {quoted} sat over {lock} blocks, not {cap} over {blocks}"
        );
    }
    Ok(offer)
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
    let response_window = RESPONSE_WINDOW
        .max(bounds.response_window)
        .max(offer.quote.graph.publication_window.saturating_add(2));
    let request_height = close_height
        .checked_sub(after_assert(&offer.quote))
        .and_then(|h| h.checked_sub(u32::from(response_window)))
        .and_then(|h| h.checked_sub(u32::from(offer.quote.graph.assert_window)))
        .filter(|a| *a > tip)
        .with_context(|| format!("{blocks} blocks leave no time to recover in"))?;
    Ok(Wanted {
        request_height,
        close_height,
        response_window,
    })
}

/// A deposit at `hub` paid `left` exactly, F's change, the hub's fee in
/// it, to pay in F at `tip`: its hash and the script F pays. Fails if
/// the hub takes none so (its fee changed since its info was read, say),
/// or leaves too little time to sign F and have it confirm
/// ([`DEPOSIT_MARGIN`]).
async fn deposit_for_change<T: Transport>(
    wallet: &Wallet<T>,
    hub: &str,
    left: u64,
    tip: u32,
) -> Result<(String, ScriptBuf)> {
    let asked = (wallet.deposit_paying(hub, left).await)
        .with_context(|| format!("{left} sat left over the price, not to be notes at {hub}"))?;
    let script = asked.terms.as_ref().and_then(|terms| {
        let time = terms.fund_by >= tip.saturating_add(2 * DEPOSIT_MARGIN);
        (time && terms.pays == left).then(|| script_of(&terms.address))?
    });
    match script {
        Some(script) => Ok((asked.hash, script)),
        None => {
            let why = "not to be paid in F: too little time, or paid another amount";
            let _ = wallet.give_up_deposit(&asked.hash, why);
            bail!("{left} sat left over the price, not to be notes at {hub}: {why}")
        }
    }
}

/// The output script of `address`, a deposit's, as the wallet recorded
/// it: on its network.
pub(crate) fn script_of(address: &str) -> Option<ScriptBuf> {
    let address: bitcoin::Address<bitcoin::address::NetworkUnchecked> = address.parse().ok()?;
    Some(address.assume_checked().script_pubkey())
}

/// The coins of `wallet` free to deposit: unspent and no other
/// contract's deposit.
fn free<T: Transport>(wallet: &Wallet<T>, store: &Store) -> Result<Vec<SilentCoin>> {
    let taken: Vec<OutPoint> = store
        .list()?
        .into_iter()
        .filter(|c| c.abandoned.is_none() && c.fate != Some(Fate::Dead))
        .map(|c| c.deposit)
        .collect();
    free_of(wallet, &taken)
}

fn free_of<T: Transport>(wallet: &Wallet<T>, taken: &[OutPoint]) -> Result<Vec<SilentCoin>> {
    let mut coins = wallet.silent_coins()?;
    coins.retain(|c| c.spending.is_none() && !taken.contains(&c.outpoint));
    Ok(coins)
}

/// The smallest of `coins` of `least` sat at least.
fn smallest(coins: Vec<SilentCoin>, least: u64) -> Option<SilentCoin> {
    coins
        .into_iter()
        .filter(|c| c.value >= least)
        .min_by_key(|c| c.value)
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
    let Some((fate, block)) = chain::fate(holder, chain, since).await? else {
        return Ok(());
    };
    driver.store.settle_fate(&driver.id, fate, block)?;
    if fate == Fate::Dead {
        driver.store.forget(&driver.id)?;
    }
    let tip = chain.height().await.map_err(|e| anyhow!(e))?;
    let free = fate == Fate::Dead || (fate == Fate::Open && holder.lapsed(tip));
    let Some(f) = holder.funding() else {
        return Ok(());
    };
    let txid = f.tx.compute_txid();
    if free {
        // Freed already, or spent: nothing to free.
        let _ = driver.wallet.abandon_silent(&txid);
        tracing::warn!(id = %crate::hex_id(&driver.id), %txid, ?fate, "F will not confirm: her coin freed");
    } else if fate == Fate::Open {
        // Her coin kept for F: a wallet rewinding past it, as a reorg
        // makes it, forgets what reserved it. Refused once a block
        // spends it, which is no matter.
        if let Err(e) = driver.wallet.sign_input(f.tx, f.prevouts, f.deposit) {
            tracing::debug!(id = %crate::hex_id(&driver.id), %txid, "her coin not kept again: {e}");
        }
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

#[cfg(test)]
mod tests {
    use bitcoin::{hashes::Hash as _, Block, BlockHash, Transaction, Txid};
    use licash::{chain::Checkpoint, spend::ChainProfile};
    use licash_wallet::{Blocks, Response};

    use super::*;

    #[test]
    fn an_opening_short_of_her_signing_f_a_day_on_is_given_up() {
        let began = 1_794_900_000;
        let day = began + OPENS_WITHIN;
        for phase in [
            Phase::Started,
            Phase::Committed,
            Phase::Chosen,
            Phase::Consented,
        ] {
            assert!(!overdue(phase, began, day), "{phase:?} within a day");
            assert!(overdue(phase, began, day + 1), "{phase:?} past a day");
        }
        // Her deposit signed into F: never given up for its age.
        for phase in [Phase::Signing, Phase::Funded] {
            assert!(!overdue(phase, began, day + 365 * 86_400), "{phase:?}");
        }
    }

    #[test]
    fn inspection_uses_the_funded_graph_not_the_deposit_and_never_adds_the_cap_twice() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = Wallet::create(
            &dir.path().join("wallet.sqlite"),
            &[7; 32],
            ChainProfile::Core30DefaultRegtest,
            Nowhere,
        )
        .unwrap();
        let keys = wallet.contract_secrets();
        let holder = Holder::load(
            &crate::testing::opened().holder,
            &keys.seed,
            &licash_contract::opening::Context {
                tip: 0,
                trust: crate::trust(ChainProfile::Core30DefaultRegtest).unwrap(),
            },
        )
        .unwrap();
        let opening = holder.signed().unwrap();
        let info = inspection(&opening, bitcoin::Network::Regtest).unwrap();
        assert_eq!(
            info.locked,
            info.payout_pool + info.holder_reserve + info.hub_reserve
        );
        assert!(u64::from(info.claim_limit) <= info.payout_pool);
        assert_eq!(
            info.note_limit,
            32.min(opening.base.parts().limits.max_authorizations)
        );
        assert_eq!(
            info.denominations,
            opening
                .base
                .parts()
                .keyset
                .denominations()
                .collect::<Vec<_>>()
        );
        let address = info
            .address
            .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()
            .unwrap()
            .require_network(bitcoin::Network::Regtest)
            .unwrap();
        assert_eq!(
            address.script_pubkey(),
            opening.babe.tree().unwrap().script_pubkey()
        );
        assert_eq!(
            info.graph_version,
            opening.base.parts().recipe.graph_version
        );
    }

    #[test]
    fn coverage_uses_live_trusted_contracts_in_deadline_and_id_order() {
        let contract = |id, deadline, fate| Contract {
            id: [id; 32],
            hub: "http://hub.example".into(),
            phase: Phase::Funded,
            mock: false,
            deposit: OutPoint::null(),
            value: 60_000,
            price: 23_000,
            cap: 4_096,
            assert_height: deadline,
            close_height: 500,
            since: 200,
            funding: None,
            fate: Some(fate),
            abandoned: None,
            created_at: 0,
            folded: None,
        };
        let mut other = contract(5, 320, Fate::Live);
        other.hub = "http://other.example".into();
        let all = [
            contract(3, 400, Fate::Live),
            contract(2, 380, Fate::Live),
            contract(1, 400, Fate::Live),
            contract(4, 350, Fate::Open),
            other,
        ];
        let held = [licash_wallet::Balance {
            hub: "http://hub.example/".into(),
            available: 104,
            pending: 1_000,
        }];
        let ids = |tip| {
            coverage_order(&all, tip, &held, &HashSet::new())
                .iter()
                .map(|c| c.id[0])
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(300), [2, 1, 3]);
        assert_eq!(ids(380), [1, 3]);
        assert!(ids(400).is_empty());
        assert!(ids(0).is_empty());
        assert_eq!(
            coverage_order(&all, 300, &held, &HashSet::from([[2; 32]]))
                .iter()
                .map(|c| c.id[0])
                .collect::<Vec<_>>(),
            [1, 3]
        );
    }

    #[test]
    fn a_restored_garbler_needs_no_instance_files_and_an_unreadable_opening_is_carried() {
        let dir = tempfile::tempdir().unwrap();
        let regtest = ChainProfile::Core30DefaultRegtest;
        let wallet = Wallet::create(
            &dir.path().join("wallet.sqlite"),
            &[7; 32],
            regtest,
            Nowhere,
        )
        .unwrap();
        // No contracts kept at all: nothing to check.
        validate_backup(&wallet, dir.path()).unwrap();
        let keys = wallet.contract_secrets();
        let ctx = licash_contract::opening::Context {
            tip: 0,
            trust: crate::trust(regtest).unwrap(),
        };
        let bytes = &crate::testing::opened().holder;
        let o = Holder::load(bytes, &keys.seed, &ctx)
            .unwrap()
            .signed()
            .unwrap();
        let id = o.base.parts().nonce;
        let store = Store::open(dir.path()).unwrap();
        let insert = |id: [u8; 32], vout: u32, state: &[u8]| {
            store
                .insert(&New {
                    id,
                    hub: "http://hub.example",
                    mock: true,
                    deposit: OutPoint {
                        txid: Txid::all_zeros(),
                        vout,
                    },
                    value: 60_000,
                    price: 23_000,
                    cap: 4_096,
                    assert_height: 300,
                    close_height: 500,
                    since: 0,
                    state,
                })
                .unwrap();
            store.save(&id, Phase::Funded, None, state).unwrap();
        };
        insert(id, 0, bytes);
        // The backed-up wallet seed regenerates the holder's circuits.
        validate_backup(&wallet, dir.path()).unwrap();
        // One this build cannot read is restored as it was, not refused.
        insert([9; 32], 1, b"of a later version");
        validate_backup(&wallet, dir.path()).unwrap();
    }

    /// No hub to reach.
    struct Nowhere;

    impl Transport for Nowhere {
        async fn request(&self, _: &str, _: Option<Vec<u8>>) -> Result<Response, String> {
            Err("no hub here".into())
        }
    }

    /// A chain at height 250, asked nothing else.
    struct Tip;

    impl Blocks for Tip {
        async fn height(&self) -> Result<u32, String> {
            Ok(250)
        }
        async fn hash(&self, _: u32) -> Result<BlockHash, String> {
            Err("not asked".into())
        }
        async fn filter(&self, _: &BlockHash) -> Result<Option<Vec<u8>>, String> {
            Err("not asked".into())
        }
        async fn block(&self, _: &BlockHash) -> Result<Block, String> {
            Err("not asked".into())
        }
    }

    impl Chain for Tip {
        async fn checkpoint(&self, _: u32) -> Result<Checkpoint, String> {
            Err("not asked".into())
        }
        async fn broadcast(&self, _: &Transaction) -> Result<(), String> {
            Err("not asked".into())
        }
    }

    #[cfg(feature = "node")]
    #[test]
    fn her_deadlines_leave_each_window_whole_on_the_hubs_terms() {
        use crate::hub::terms::{Timing, LOCK};
        let keyset = licash::note::IssuerKeys::from_seed(&[1; 32], 0, 4).unwrap();
        let keyset = keyset.keyset().unwrap();
        let regtest = ChainProfile::Core30DefaultRegtest;
        let signet = ChainProfile::Core30Signet;
        for profile in [regtest, signet] {
            let timing = Timing::of(profile, None).unwrap();
            let checkpoint = Checkpoint::genesis(profile).unwrap();
            let tip = checkpoint.height();
            let quote = timing.quote(keyset.clone(), checkpoint);
            let offer = Offer {
                bounds: timing.bounds(&quote),
                quote,
            };
            offer.quote.check(profile).unwrap();
            let wanted = wanted(&offer, tip, LOCK).unwrap();
            offer.bounds.check(&wanted, tip).unwrap();
            let (g, c) = (&offer.quote.graph, &offer.quote.chain);
            let window = u32::from(wanted.response_window);
            // After the assertion deadline: the hub's challenge, her answer
            // and the delays on what she is paid, whole.
            let after = [g.challenge_window, g.answer_window, g.delta, g.delta_prime];
            let after: u32 = after.map(u32::from).iter().sum();
            let gap = wanted.close_height - wanted.request_height;
            assert!(gap > after, "{profile}: {gap} blocks after asserting");
            // Before it, Base's funding gate: F and a request deep enough,
            // her outputs' delay and the window to respond.
            let delay = u32::from(g.to_self_delay);
            let gate = c.funding_depth + delay + c.work_depth + window + g.deadline_margin;
            assert!(wanted.request_height - tip >= gate, "{profile}");
            // A publication confirmed deep enough within the window.
            assert!(window > u32::from(g.publication_window) + 1, "{profile}");
        }
        // Regtest's as before; signet's 180 blocks after asserting, of
        // its 1 000, and 12 to respond.
        let gap = |profile| {
            let timing = Timing::of(profile, None).unwrap();
            let quote = timing.quote(keyset.clone(), Checkpoint::genesis(profile).unwrap());
            (after_assert(&quote), timing.bounds(&quote).response_window)
        };
        assert_eq!(gap(regtest), (AFTER_ASSERT, 2));
        assert_eq!(gap(signet), (180, 12));
        for profile in [ChainProfile::Core30Mainnet, ChainProfile::Mutinynet] {
            assert!(Timing::of(profile, None).is_err());
        }
    }

    /// Of terms she chose within the hub's range, the fewest blocks it
    /// lets her ask for and the most leave each window whole too, on a
    /// quote as old as the hub takes one (6 blocks).
    #[cfg(feature = "node")]
    #[test]
    fn her_deadlines_leave_each_window_whole_on_terms_she_chose() {
        use crate::hub::terms::{Range, Timing, CAP};
        let keyset = licash::note::IssuerKeys::from_seed(&[1; 32], 0, 4).unwrap();
        let keyset = keyset.keyset().unwrap();
        let range = Range {
            max_cap: 100_000_000,
            max_blocks: 26_280,
            per_coin: 2,
            ppm_year: 10_000,
        };
        for profile in [
            ChainProfile::Core30DefaultRegtest,
            ChainProfile::Core30Signet,
        ] {
            let timing = Timing::of(profile, Some(range)).unwrap();
            let r = timing.range().unwrap();
            let checkpoint = Checkpoint::genesis(profile).unwrap();
            for (cap, blocks) in [
                (CAP, r.min_blocks),
                (15_000_000, 26_280),
                (r.max_cap, r.max_blocks),
            ] {
                let quote = timing.quote_for(keyset.clone(), checkpoint.clone(), cap, blocks);
                quote.check(profile).unwrap();
                let offer = Offer {
                    bounds: timing.bounds(&quote),
                    quote,
                };
                let (g, c) = (&offer.quote.graph, &offer.quote.chain);
                for age in [0, 6] {
                    let tip = checkpoint.height() + age;
                    let wanted = wanted(&offer, tip, blocks).unwrap();
                    offer.bounds.check(&wanted, tip).unwrap();
                    // Her headers reach the assertion deadline.
                    assert!(checkpoint.height() + c.max_headers >= wanted.request_height);
                    let window = u32::from(wanted.response_window);
                    let delay = u32::from(g.to_self_delay);
                    let gate = c.funding_depth + delay + c.work_depth + window + g.deadline_margin;
                    let before = wanted.request_height - tip;
                    assert!(before >= gate, "{profile} {blocks}: {before} blocks");
                }
            }
            // One block fewer than the least leaves less than that.
            assert!(timing.allows(CAP, r.min_blocks - 1).is_err());
        }
    }

    #[tokio::test]
    async fn an_opening_that_cannot_be_loaded_holds_up_no_other() {
        let dir = tempfile::tempdir().unwrap();
        let regtest = ChainProfile::Core30DefaultRegtest;
        let path = dir.path().join("wallet.sqlite");
        let wallet = Wallet::create(&path, &[7; 32], regtest, Nowhere).unwrap();
        let store = Store::open(dir.path()).unwrap();
        let new = |id: u8, vout: u32| New {
            id: [id; 32],
            hub: "http://hub.example",
            mock: true,
            deposit: OutPoint {
                txid: Txid::from_byte_array([9; 32]),
                vout,
            },
            value: 60_000,
            price: 23_000,
            cap: 4_096,
            assert_height: 360,
            close_height: 500,
            since: 200,
            state: b"not a state",
        };
        // One she never signed F for, then one she did: neither loads.
        store.insert(&new(1, 0)).unwrap();
        store.insert(&new(2, 1)).unwrap();
        let f = Some(Txid::from_byte_array([8; 32]));
        store.save(&[2; 32], Phase::Funded, f, b"nor this").unwrap();

        let contracts = resume(&wallet, &Tip, dir.path()).await.unwrap();
        let of = |id: u8| contracts.iter().find(|c| c.id == [id; 32]).unwrap();
        // The first given up, its coin free again; the other kept as it is.
        let reason = of(1).abandoned.as_deref().unwrap();
        assert!(reason.starts_with("cannot be carried on"), "{reason}");
        assert_eq!(of(2).abandoned, None);
        assert_eq!(of(2).phase, Phase::Funded);
        // Not read again at each sync of the app: one found unreadable
        // is passed over, even one she never signed F for that loading
        // would give up.
        assert!(unreadable().contains(&[2; 32]));
        store.insert(&new(3, 2)).unwrap();
        unreadable().insert([3; 32]);
        let again = resume(&wallet, &Tip, dir.path()).await.unwrap();
        assert_eq!(
            again.iter().find(|c| c.id == [3; 32]).unwrap().abandoned,
            None
        );
    }

    /// A chain made up to `tip`, of the fork `fork`, with `f` in the block
    /// at `at` if given: what is broadcast kept, the hashes asked counted.
    struct Fork {
        tip: u32,
        fork: u32,
        f: Option<(u32, Transaction)>,
        asked: std::sync::atomic::AtomicU32,
        sent: std::sync::Mutex<Vec<Transaction>>,
    }

    impl Fork {
        fn new(tip: u32, fork: u32, f: Option<(u32, &Transaction)>) -> Self {
            Self {
                tip,
                fork,
                f: f.map(|(at, tx)| (at, tx.clone())),
                asked: Default::default(),
                sent: Default::default(),
            }
        }

        fn at(&self, height: u32) -> Block {
            let mut header = bitcoin::constants::genesis_block(bitcoin::Network::Regtest).header;
            (header.nonce, header.time) = (height, self.fork);
            let txdata = match &self.f {
                Some((at, tx)) if *at == height => vec![tx.clone()],
                _ => vec![],
            };
            Block { header, txdata }
        }
    }

    impl Blocks for Fork {
        async fn height(&self) -> Result<u32, String> {
            Ok(self.tip)
        }
        async fn hash(&self, height: u32) -> Result<BlockHash, String> {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match height <= self.tip {
                true => Ok(self.at(height).block_hash()),
                false => Err(format!("no block {height}")),
            }
        }
        async fn filter(&self, _: &BlockHash) -> Result<Option<Vec<u8>>, String> {
            Ok(None)
        }
        async fn block(&self, hash: &BlockHash) -> Result<Block, String> {
            (0..=self.tip)
                .map(|h| self.at(h))
                .find(|b| b.block_hash() == *hash)
                .ok_or_else(|| format!("no block {hash}"))
        }
    }

    impl Chain for Fork {
        async fn checkpoint(&self, _: u32) -> Result<Checkpoint, String> {
            Err("not asked".into())
        }
        async fn broadcast(&self, tx: &Transaction) -> Result<(), String> {
            self.sent.lock().unwrap().push(tx.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_live_f_whose_block_leaves_the_chain_is_watched_again_until_buried() {
        let dir = tempfile::tempdir().unwrap();
        let regtest = ChainProfile::Core30DefaultRegtest;
        let path = dir.path().join("wallet.sqlite");
        let wallet = Wallet::create(&path, &[7; 32], regtest, Nowhere).unwrap();
        let bytes = &crate::testing::opened().holder;
        let ctx = licash_contract::opening::Context {
            tip: 0,
            trust: crate::trust(regtest).unwrap(),
        };
        let holder = Holder::load(bytes, &wallet.contract_secrets().seed, &ctx).unwrap();
        let f = holder.funding().unwrap().tx.clone();
        let id = holder.signed().unwrap().base.parts().nonce;
        let store = Store::open(dir.path()).unwrap();
        store
            .insert(&New {
                id,
                hub: "http://hub.example",
                mock: true,
                deposit: f.input[0].previous_output,
                value: 60_000,
                price: 23_000,
                cap: 4_096,
                assert_height: 300,
                close_height: 500,
                since: 240,
                state: bytes,
            })
            .unwrap();
        store
            .save(&id, Phase::Funded, Some(f.compute_txid()), bytes)
            .unwrap();
        async fn fate_of(
            wallet: &Wallet<Nowhere>,
            chain: &Fork,
            dir: &Path,
            id: [u8; 32],
        ) -> Option<Fate> {
            let all = resume(wallet, chain, dir).await.unwrap();
            all.iter().find(|c| c.id == id).unwrap().fate
        }
        let fate = |chain| fate_of(&wallet, chain, dir.path(), id);

        // Found live by a build before: trusted, its chain not asked.
        store.set_fate(&id, Fate::Live).unwrap();
        let chain = Fork::new(250, 0, Some((247, &f)));
        assert_eq!(fate(&chain).await, Some(Fate::Live));
        assert_eq!(chain.asked.load(std::sync::atomic::Ordering::SeqCst), 0);

        // Found live in its block: followed while that block stands,
        // until buried.
        store.set_fate(&id, Fate::Open).unwrap();
        assert_eq!(fate(&chain).await, Some(Fate::Live));
        let block = (247, chain.at(247).block_hash());
        assert_eq!(store.fate_block(&id).unwrap(), Some(block));
        // Not carried on: F not broadcast again.
        chain.sent.lock().unwrap().clear();
        assert_eq!(fate(&chain).await, Some(Fate::Live));
        assert_eq!(store.fate_block(&id).unwrap(), Some(block));
        assert!(chain.sent.lock().unwrap().is_empty());

        // Its block replaced, F in none: open again, and broadcast again.
        let replaced = Fork::new(250, 1, None);
        assert_eq!(fate(&replaced).await, Some(Fate::Open));
        assert_eq!(store.fate_block(&id).unwrap(), None);
        assert_eq!(*replaced.sent.lock().unwrap(), std::slice::from_ref(&f));

        // Mined again in another block: live, followed there; buried six
        // deep, no longer.
        let again = Fork::new(250, 1, Some((249, &f)));
        assert_eq!(fate(&again).await, Some(Fate::Live));
        let block = (249, again.at(249).block_hash());
        assert_eq!(store.fate_block(&id).unwrap(), Some(block));
        let buried = Fork::new(254, 1, Some((249, &f)));
        assert_eq!(fate(&buried).await, Some(Fate::Live));
        assert_eq!(store.fate_block(&id).unwrap(), None);
        // Then never looked at again.
        let deep = Fork::new(260, 2, None);
        assert_eq!(fate(&deep).await, Some(Fate::Live));
        assert_eq!(deep.asked.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
