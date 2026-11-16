//! The holder's side of openings, in her wallet: [`open`] a contract at
//! a hub she trusts, [`resume`] those under way and watch their F, and
//! [`list`] them. Her deposit is a coin of the wallet's, signed into F by
//! it; her change returns to its fallback address, or into notes at the
//! hub ([`Change::Notes`]). One her hub left silent she forces closed, in
//! [`recovery`].

mod chain;
mod driver;
#[cfg(feature = "node")]
mod node;
pub mod recovery;
mod store;

use std::path::Path;

use anyhow::{anyhow, ensure, Context as _, Result};
use bitcoin::{OutPoint, ScriptBuf};
use licash_contract::opening::{Holder, Quote, Start};
use licash_contract::wire::Wanted;
use licash_wallet::{SilentCoin, Transport, Wallet};

pub use chain::Chain;
use driver::Driver;
pub use licash_contract::opening::{Fate, Phase};
#[cfg(feature = "node")]
pub use node::Node;
pub use store::{kept, Contract};
use store::{New, Store};

/// How many of the hub's instances an opening keeps: of each stage,
/// those she chose, downloaded whole before she consents.
pub const KEEPS: usize = licash_contract::wire::STAGES * licash_contract::KEPT;

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
    let windows: u32 = [g.hub_challenge, g.holder_answer, g.delta, g.delta_prime]
        .map(u32::from)
        .iter()
        .sum();
    (windows + SPARE).max(AFTER_ASSERT)
}

/// The blocks she has to respond to the hub's request, at least.
pub const RESPONSE_WINDOW: u16 = 6;

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
    /// it, the hub's fee in it: no change left on Bitcoin. On Bitcoin
    /// all the same where the hub takes no deposit paid so much exactly,
    /// or leaves too little time to pay it in.
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
/// from her first message on.
pub async fn open<T: Transport, C: Chain>(
    wallet: &Wallet<T>,
    chain: &C,
    dir: &Path,
    hub: &str,
    ask: Ask,
) -> Result<Contract> {
    open_with(wallet, chain, dir, hub, ask, Change::Bitcoin).await
}

/// [`open`], F's `change` going where asked: into notes at the hub, F
/// sweeping her coin whole, if it can.
pub async fn open_with<T: Transport, C: Chain>(
    wallet: &Wallet<T>,
    chain: &C,
    dir: &Path,
    hub: &str,
    ask: Ask,
    change: Change,
) -> Result<Contract> {
    let hub = hub.trim_end_matches('/').to_owned();
    let trust = crate::trust(wallet.network())?;
    let trusted = wallet.hubs()?;
    let (_, info) = trusted
        .iter()
        .find(|(url, _)| *url == hub)
        .with_context(|| format!("{hub} is no hub this wallet trusts"))?;
    let chosen = ask.cap.map(|cap| (cap, ask.blocks));
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
        Change::Notes if left >= offer.quote.graph.dust && window >= 2 * DEPOSIT_MARGIN => {
            deposit_for_change(wallet, &hub, left, tip).await
        }
        _ => None,
    };
    let secrets = wallet.contract_secrets();
    let start = Start {
        seed: secrets.seed,
        spend: secrets.spend,
        quote: offer.quote.clone(),
        deposit,
        coin: coin.clone(),
        change: match &into {
            Some((_, script)) => script.clone(),
            None => wallet.fallback_address()?.script_pubkey(),
        },
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
    let (holder, outs) = match Holder::new(start, &ctx) {
        Ok(started) => started,
        Err(e) => {
            give_up("its opening never started");
            return Err(e);
        }
    };
    let id = holder.id();
    let inserted = store.insert(&New {
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
    });
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
        let holder = match Holder::load(&state, secrets.seed, secrets.spend, &ctx) {
            Ok(holder) => holder,
            Err(e) => {
                // One that cannot be read (opened under programs this build
                // does not name, say) holds up none of the others: given up
                // if she never signed its F, else kept as it is.
                tracing::warn!(id = %crate::hex_id(&id), "loading an opening: {e:#}");
                store.abandon(&id, &format!("cannot be carried on: {e}"))?;
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
        let holder = match Holder::load(&state, secrets.seed, secrets.spend, &ctx) {
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
    let assert_height = close_height
        .checked_sub(after_assert(&offer.quote))
        .filter(|a| *a > tip)
        .with_context(|| format!("{blocks} blocks leave no time to recover in"))?;
    Ok(Wanted {
        assert_height,
        close_height,
        response_window: RESPONSE_WINDOW.max(bounds.response_window),
    })
}

/// A deposit at `hub` paid `left` exactly, F's change, the hub's fee in
/// it, to pay in F at `tip`: its hash and the script F pays. None if the
/// hub takes none so (its fee changed since its info was read, say), or
/// leaves too little time to sign F and have it confirm
/// ([`DEPOSIT_MARGIN`]), its change then on Bitcoin.
async fn deposit_for_change<T: Transport>(
    wallet: &Wallet<T>,
    hub: &str,
    left: u64,
    tip: u32,
) -> Option<(String, ScriptBuf)> {
    let asked = match wallet.deposit_paying(hub, left).await {
        Ok(asked) => asked,
        Err(e) => {
            tracing::info!(%hub, left, "F's change stays on Bitcoin: {e}");
            return None;
        }
    };
    let script = asked.terms.as_ref().and_then(|terms| {
        let time = terms.fund_by >= tip.saturating_add(2 * DEPOSIT_MARGIN);
        (time && terms.pays == left).then(|| script_of(&terms.address))?
    });
    if script.is_none() {
        let why = "not to be paid in F: too little time, or paid another amount";
        tracing::info!(%hub, left, hash = %asked.hash, "F's change stays on Bitcoin: {why}");
        let _ = wallet.give_up_deposit(&asked.hash, why);
    }
    Some((asked.hash, script?))
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
    let Some(fate) = chain::fate(holder, chain, since).await? else {
        return Ok(());
    };
    driver.store.set_fate(&driver.id, fate)?;
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
            let after = [g.hub_challenge, g.holder_answer, g.delta, g.delta_prime];
            let after: u32 = after.map(u32::from).iter().sum();
            let gap = wanted.close_height - wanted.assert_height;
            assert!(gap > after, "{profile}: {gap} blocks after asserting");
            // Before it, Base's funding gate: F and a request deep enough,
            // her outputs' delay and the window to respond.
            let delay = u32::from(g.to_self_delay);
            let gate = c.funding_depth + delay + c.request_depth + window + g.deadline_margin;
            assert!(wanted.assert_height - tip >= gate, "{profile}");
            // A publication confirmed deep enough within the window.
            assert!(
                window > c.publication_depth + c.observation_margin,
                "{profile}"
            );
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
                    assert!(checkpoint.height() + c.max_headers >= wanted.assert_height);
                    let window = u32::from(wanted.response_window);
                    let delay = u32::from(g.to_self_delay);
                    let gate =
                        c.funding_depth + delay + c.request_depth + window + g.deadline_margin;
                    let before = wanted.assert_height - tip;
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
    }
}
