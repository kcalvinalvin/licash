//! Recovery contracts with the hubs trusted, opened from a coin of the
//! wallet's on Bitcoin, carried on and watched as the chain moves: each
//! protects the wallet's notes at its hub, up to its cap, until its last
//! block to recover in; its collateral returns to the hub at its close.
//! There is no closing one sooner: the notes it protects are cashed out
//! instead, and it lapses.
//!
//! Kept in the wallet's directory, `contracts.sqlite` and `contracts/`,
//! made only once a first contract is asked for.

use std::{
    collections::HashMap,
    path::Path,
    sync::{atomic::Ordering, Arc, PoisonError},
};

use bitcoin::{OutPoint, Txid};
use licash_contracts::holder::{self, Ask, Fate, Phase};

use super::Wallet;
use crate::{
    chain::Chain, Activity, Balance, ChainSource, Contract, ContractQuote, ContractState,
    DepositCoin, Details, Kind, LicashError, OpeningStep, State,
};

/// What opening a contract downloads and keeps, in bytes, about: the
/// hub's instances its holder keeps, four of 25 MB.
const DOWNLOAD: u64 = 100_100_000;

/// How long a contract being opened keeps its coin from moving into
/// Licash, in seconds: the hub gives up on one idle an hour, and a coin
/// kept longer would never move.
const OPENING: i64 = 86_400;

/// Blocks a contract chosen lasts at first: half a year.
const HALF_YEAR: u32 = 26_280;

/// The file in the wallet's directory naming the coins paying for
/// contracts whose change moves into Licash once their funding confirms,
/// one `txid:vout` a line: none without it.
const CHANGES: &str = "contracts-change";

/// Recovery contracts.
#[uniffi::export]
impl Wallet {
    /// What opening a contract at `hub`, a hub trusted, costs now, as it
    /// offers, and the coin on Bitcoin that would pay it.
    pub fn contract_quote(&self, hub: String) -> Result<ContractQuote, LicashError> {
        let (hub, terms) = self.contracting_hub(&hub)?;
        let trust = licash_contracts::trust(self.wallet.network());
        let trust = trust.map_err(|e| LicashError::invalid(format!("{e:#}")))?;
        let offer = holder::offer(&self.wallet, &hub, None);
        let offer = self.runtime.block_on(offer).map_err(unavailable)?;
        let price = offer.price();
        let coin = holder::coin(&self.wallet, &self.dir, price).map_err(failed)?;
        let coins = holder::coins(&self.wallet, &self.dir).map_err(failed)?;
        let blocks = offer.bounds.lock;
        let after_assert = holder::after_assert(&offer.quote);
        let largest = coins.first().map(|c| c.value);
        let (suggested_cap, suggested_blocks) = match &terms.range {
            Some(range) => suggested(range, largest),
            None => (offer.quote.cap, blocks),
        };
        Ok(ContractQuote {
            hub,
            price,
            cap: u64::from(offer.quote.cap),
            blocks,
            protects_for: blocks.saturating_sub(after_assert),
            mock: trust.mock,
            coin: coin.map(|c| c.value),
            download: DOWNLOAD,
            range: terms.range.map(Into::into),
            after_assert,
            coins: coins
                .iter()
                .map(|c| DepositCoin {
                    outpoint: c.outpoint.to_string(),
                    value: c.value,
                })
                .collect(),
            suggested_cap: u64::from(suggested_cap),
            suggested_blocks,
        })
    }

    /// Open a contract at `hub`, a hub trusted, closing `blocks` from now
    /// (by default, as late as it allows), for at most `max_price` (by
    /// default, what its info says), broadcasting its funding through
    /// `chain`: the contract, confirming once its funding is signed.
    /// Paid from the smallest coin on Bitcoin that pays it, its change
    /// back to the wallet. Takes minutes: its downloads are large. One at
    /// a time; under way, [`Wallet::resume_contracts`] carries it on.
    pub fn open_contract(
        &self,
        chain: Arc<dyn ChainSource>,
        hub: String,
        blocks: Option<u32>,
        max_price: Option<u64>,
    ) -> Result<Contract, LicashError> {
        let (hub, terms) = self.contracting_hub(&hub)?;
        let ask = Ask {
            blocks: blocks.unwrap_or(terms.blocks),
            max_price: max_price.unwrap_or(terms.price),
            cap: None,
            deposit: None,
        };
        let _contracting = self
            .contracting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.opened(chain, &hub, ask)
    }

    /// Open a contract at `hub`, a hub trusted that lets the wallet
    /// choose (its terms' range), protecting up to `cap` sat and closing
    /// `blocks` from now, for at most `max_price`, paid from `coin`
    /// (`txid:vout`; by default the smallest large enough), as
    /// [`Wallet::open_contract`]. With `move_change`, its change moves
    /// into Licash at the hub once its funding confirms
    /// ([`Wallet::move_changes`]).
    #[allow(clippy::too_many_arguments)]
    pub fn open_chosen_contract(
        &self,
        chain: Arc<dyn ChainSource>,
        hub: String,
        cap: u64,
        blocks: u32,
        max_price: u64,
        coin: Option<String>,
        move_change: bool,
    ) -> Result<Contract, LicashError> {
        let (hub, terms) = self.contracting_hub(&hub)?;
        let Some(range) = terms.range else {
            return Err(LicashError::invalid(format!(
                "{hub} offers contracts of {} sat over {} blocks only",
                terms.cap, terms.blocks
            )));
        };
        let cap = u32::try_from(cap)
            .map_err(|_| LicashError::invalid(format!("a cap of {cap} sat: too large")))?;
        let _contracting = self
            .contracting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let deposit = match coin {
            Some(coin) => coin
                .parse::<OutPoint>()
                .map_err(|e| LicashError::invalid(format!("a coin {coin}: {e}")))?,
            None => {
                let least = holder::least(Some(range), cap, max_price);
                let coin = holder::coin(&self.wallet, &self.dir, least).map_err(failed)?;
                let coin = coin.ok_or_else(|| {
                    LicashError::invalid(format!(
                        "no coin on Bitcoin of {least} sat at least to pay for it"
                    ))
                })?;
                coin.outpoint
            }
        };
        let ask = Ask {
            blocks,
            max_price,
            cap: Some(cap),
            deposit: Some(deposit),
        };
        if move_change {
            // Before the opening starts: one cut short carries on.
            let mut changes = self.changes()?;
            if !changes.contains(&deposit) {
                changes.push(deposit);
                self.set_changes(&changes)?;
            }
        }
        let opened = self.opened(chain, &hub, ask);
        if opened.is_err() && move_change {
            // None started: no change to move.
            let all = holder::list(&self.dir).map_err(failed)?;
            if !all.iter().any(|c| c.deposit == deposit) {
                let changes = self.changes()?.into_iter().filter(|c| *c != deposit);
                self.set_changes(&changes.collect::<Vec<_>>())?;
            }
        }
        opened
    }

    /// Move the change of the contracts opened to move it, once their
    /// funding confirms, into Licash at their hubs (or another hub
    /// trusted taking coins, if theirs does not), broadcasting through
    /// `chain`: the moves under way. None while a contract is being
    /// opened.
    pub fn move_changes(&self, chain: Arc<dyn ChainSource>) -> Result<Vec<Activity>, LicashError> {
        let changes = self.changes()?;
        if changes.is_empty() {
            return Ok(Vec::new());
        }
        let Ok(_contracting) = self.contracting.try_lock() else {
            return Ok(Vec::new());
        };
        let all = holder::list(&self.dir).map_err(failed)?;
        let coins = self.wallet.silent_coins()?;
        let mut left = Vec::new();
        let mut moved = Vec::new();
        for deposit in changes {
            match change(&all, &coins, &deposit) {
                Change::Wait => left.push(deposit),
                Change::None => {
                    tracing::info!(%deposit, "no change of a contract's to move");
                }
                Change::Move(coin, hub) => {
                    let keep: Vec<OutPoint> = coins
                        .iter()
                        .map(|c| c.outpoint)
                        .filter(|c| *c != coin)
                        .collect();
                    match self.move_keeping(chain.clone(), &keep, Some(hub)) {
                        Ok(activity) => moved.push(activity),
                        Err(e) if retried(&e) => {
                            tracing::warn!(%deposit, "moving a contract's change: {e}");
                            left.push(deposit);
                        }
                        Err(e) => {
                            tracing::info!(%deposit, "a contract's change stays on Bitcoin: {e}");
                        }
                    }
                }
            }
        }
        self.set_changes(&left)?;
        Ok(moved)
    }

    /// The contracts, the newest first, as last seen.
    pub fn contracts(&self) -> Result<Vec<Contract>, LicashError> {
        let all = holder::list(&self.dir).map_err(failed)?;
        self.listed(&all)
    }

    /// Carry on with the contracts being opened, see where the funding of
    /// each signed stands on `chain`, and carry on those forced closed:
    /// every contract, the newest first. While one is being opened, as
    /// last seen.
    pub fn resume_contracts(
        &self,
        chain: Arc<dyn ChainSource>,
    ) -> Result<Vec<Contract>, LicashError> {
        let Ok(_contracting) = self.contracting.try_lock() else {
            return self.contracts();
        };
        let chain = Chain(chain, self.wallet.network());
        if let Ok(tip) = self.runtime.block_on(licash_wallet::Blocks::height(&chain)) {
            self.saw(tip);
        }
        let all = holder::resume(&self.wallet, &chain, &self.dir);
        let all = self.runtime.block_on(all);
        // Carried on even if the others could not be: a forced close
        // has deadlines of its own.
        self.advance_recoveries(&chain);
        self.listed(&all.map_err(unavailable)?)
    }
}

impl Wallet {
    /// Open a contract at `hub` as `ask`ed, through `chain`, the wallet
    /// contracting.
    fn opened(
        &self,
        chain: Arc<dyn ChainSource>,
        hub: &str,
        ask: Ask,
    ) -> Result<Contract, LicashError> {
        let chain = Chain(chain, self.wallet.network());
        let tip = self.runtime.block_on(licash_wallet::Blocks::height(&chain));
        self.saw(tip.map_err(|reason| LicashError::Unavailable { reason })?);
        let opened = holder::open(&self.wallet, &chain, &self.dir, hub, ask);
        let opened = self.runtime.block_on(opened).map_err(unavailable)?;
        // Protecting nothing before its funding is buried.
        Ok(self.contract(&opened, 0))
    }

    /// The coins paying for contracts whose change is to move into
    /// Licash ([`changes`]).
    fn changes(&self) -> Result<Vec<OutPoint>, LicashError> {
        changes(&self.dir)
    }

    fn set_changes(&self, changes: &[OutPoint]) -> Result<(), LicashError> {
        set_changes(&self.dir, changes)
    }

    /// `hub`, a hub trusted that opens contracts, by its URL as contracts
    /// keep it, and its terms as its info says.
    fn contracting_hub(&self, hub: &str) -> Result<(String, licash_api::Contracts), LicashError> {
        let hub = hub.trim().trim_end_matches('/');
        let trusted = self.wallet.hubs()?.into_iter();
        let mut trusted = trusted.filter(|(url, _)| url.trim_end_matches('/') == hub);
        let Some((_, info)) = trusted.next() else {
            return Err(LicashError::invalid(format!("{hub} is no hub you use")));
        };
        match info.contracts {
            Some(terms) => Ok((hub.to_owned(), terms)),
            None => Err(LicashError::invalid(format!("{hub} opens no contracts"))),
        }
    }

    /// The chain's height, as last seen at least: 0 if never.
    pub(super) fn saw(&self, tip: u32) {
        self.tip.fetch_max(tip, Ordering::Relaxed);
    }

    /// The chain's height as the wallet last saw it, or the last block it
    /// looked in: 0 if neither is known.
    pub(super) fn tip(&self) -> u32 {
        let looked = self.wallet.silent_next().ok().flatten();
        let looked = looked.map_or(0, |next| next.saturating_sub(1));
        self.tip.load(Ordering::Relaxed).max(looked)
    }

    /// `all`, each with what it protects of the notes held now.
    fn listed(&self, all: &[holder::Contract]) -> Result<Vec<Contract>, LicashError> {
        let covers = covers(all, self.tip(), &self.wallet.balance()?);
        let covered = |c: &holder::Contract| covers.get(&c.id).copied().unwrap_or(0);
        Ok(all.iter().map(|c| self.contract(c, covered(c))).collect())
    }

    fn contract(&self, c: &holder::Contract, covers: u64) -> Contract {
        let (tip, state) = (self.tip(), state(c, self.tip()));
        let step = match state {
            ContractState::Opening => Some(opening_step(c.phase, holder::kept(&self.dir, &c.id))),
            ContractState::Confirming => {
                // As deep as the wallet saw her coin's spend: F, which
                // spends it.
                let spent_at = self.wallet.silent_spent_at(&c.deposit).ok().flatten();
                let confirmations = confirmations(spent_at, tip);
                Some(OpeningStep::Confirming { confirmations })
            }
            _ => None,
        };
        Contract {
            id: hex(&c.id),
            hub: c.hub.clone(),
            state,
            step,
            mock: c.mock,
            cap: u64::from(c.cap),
            covers,
            price: c.price,
            paid_from: c.value,
            protects_until: c.assert_height,
            closes_at: c.close_height,
            funding: c.funding.map(|t| t.to_string()),
            created_at: c.created_at,
        }
    }

    /// Of `balance`, `held` at each hub, what contracts protect, and what
    /// the contracts being opened hold on Bitcoin: its `covered` and
    /// `opening`. `reserved` are the coins of the wallet's that pending
    /// transactions spend, by those transactions' ids, with their values.
    pub(super) fn contracted(
        &self,
        balance: &mut Balance,
        held: &[licash_wallet::Balance],
        reserved: &[(Txid, u64)],
    ) -> Result<(), LicashError> {
        let all = holder::list(&self.dir).map_err(failed)?;
        if !all.is_empty() {
            contracted(balance, &all, self.tip(), held, reserved);
        }
        Ok(())
    }

    /// The contracts signed, as lines of the history: what each cost,
    /// under way until its funding is in a block.
    pub(super) fn contract_activities(&self) -> Result<Vec<Activity>, LicashError> {
        let all = holder::list(&self.dir).map_err(failed)?;
        Ok(all.iter().filter_map(activity).collect())
    }

    /// More of the line of the history of the contract `id`, if any.
    pub(super) fn contract_details(&self, id: &str) -> Result<Option<Details>, LicashError> {
        let all = holder::list(&self.dir).map_err(failed)?;
        let found = all.into_iter().find(|c| hex(&c.id) == id);
        Ok(found.and_then(|c| {
            Some(Details::Contract {
                hub: c.hub,
                funding: c.funding?.to_string(),
            })
        }))
    }

    /// The transactions funding contracts, those signed: their change is
    /// the wallet's, to keep on Bitcoin.
    pub(super) fn fundings(&self) -> Result<Vec<Txid>, LicashError> {
        let all = holder::list(&self.dir).map_err(failed)?;
        Ok(all.iter().filter_map(|c| c.funding).collect())
    }

    /// The coins contracts being opened will deposit, not reserved for
    /// their funding yet: no move is to take them. Those asked for long
    /// ago, their hub surely gone, keep none.
    pub(super) fn opening_coins(&self) -> Result<Vec<OutPoint>, LicashError> {
        let all = holder::list(&self.dir).map_err(failed)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let opening = all.iter().filter(|c| {
            c.abandoned.is_none() && c.phase < Phase::Funded && now - c.created_at < OPENING
        });
        Ok(opening.map(|c| c.deposit).collect())
    }
}

/// The coins paying for contracts whose change is to move into Licash, as
/// kept in `dir` ([`CHANGES`]): none if it was never asked.
fn changes(dir: &Path) -> Result<Vec<OutPoint>, LicashError> {
    let text = match std::fs::read_to_string(dir.join(CHANGES)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(LicashError::failed(format!("{CHANGES}: {e}"))),
    };
    // A line that does not read names no coin: nothing moves for it.
    Ok(text.lines().filter_map(|l| l.trim().parse().ok()).collect())
}

/// Keep `changes` in `dir`, as [`changes`] reads them: whole or not at all.
fn set_changes(dir: &Path, changes: &[OutPoint]) -> Result<(), LicashError> {
    let path = dir.join(CHANGES);
    let failed = |e: std::io::Error| LicashError::failed(format!("{CHANGES}: {e}"));
    if changes.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(failed(e)),
            _ => Ok(()),
        };
    }
    let text: String = changes.iter().map(|c| format!("{c}\n")).collect();
    let next = path.with_extension("next");
    std::fs::write(&next, text).map_err(failed)?;
    std::fs::rename(&next, &path).map_err(failed)
}

/// What a contract chosen within `range` is at first, paid from a coin of
/// `coin` sat, if any: half again the coin, its change moved into Licash
/// and as much again to receive, over half a year; within the range and
/// the times the coin it may be.
fn suggested(range: &licash_api::ContractRange, coin: Option<u64>) -> (u32, u32) {
    let blocks = HALF_YEAR.clamp(range.min_blocks, range.max_blocks);
    let most = coin.and_then(|c| range.max_cap_for(c).map(|most| (c, most)));
    let cap = match most {
        Some((coin, most)) => {
            let half_again = coin.saturating_add(coin / 2);
            u32::try_from(half_again)
                .unwrap_or(u32::MAX)
                .clamp(range.min_cap, most)
        }
        None => range.min_cap,
    };
    (cap, blocks)
}

/// What becomes of the change of the contract paid from `deposit`.
#[derive(Debug, PartialEq, Eq)]
enum Change<'a> {
    /// Its funding not confirmed yet, or its opening not started.
    Wait,
    /// Nothing to move: its opening given up, or its change spent or none.
    None,
    /// Move this coin into Licash at this hub.
    Move(OutPoint, &'a str),
}

/// Whether moving a contract's change that failed with `e` is tried
/// again next round: not if it never can be, as when the change is less
/// than the hub takes, so it stays on Bitcoin.
fn retried(e: &LicashError) -> bool {
    !matches!(
        e,
        LicashError::Insufficient { .. } | LicashError::Invalid { .. }
    )
}

/// What becomes of the change of the contract of `all` paid from
/// `deposit`, the wallet's `coins` as they are.
fn change<'a>(
    all: &'a [holder::Contract],
    coins: &[licash_wallet::SilentCoin],
    deposit: &OutPoint,
) -> Change<'a> {
    // The newest paid from it: one given up may have been before.
    let Some(c) = all.iter().find(|c| c.deposit == *deposit) else {
        // Its opening never started: none is under way as this is asked.
        return Change::None;
    };
    if c.abandoned.is_some() || c.fate == Some(Fate::Dead) {
        return Change::None;
    }
    let Some(funding) = c.funding else {
        return Change::Wait;
    };
    match coins.iter().find(|k| k.outpoint.txid == funding) {
        Some(coin) if coin.spending.is_none() => Change::Move(coin.outpoint, &c.hub),
        Some(_) => Change::None,
        // Seen once its funding is buried and scanned: none then, or spent.
        None if c.fate == Some(Fate::Live) => Change::None,
        None => Change::Wait,
    }
}

/// Of `balance`, `held` at each hub, what `contracts` protect at `tip`
/// ([`covers`]), and what those being opened hold of the coins `reserved`
/// for transactions, by their ids: its `covered`, and its `opening` rather
/// than `moving`.
fn contracted(
    balance: &mut Balance,
    contracts: &[holder::Contract],
    tip: u32,
    held: &[licash_wallet::Balance],
    reserved: &[(Txid, u64)],
) {
    balance.covered += covers(contracts, tip, held).values().sum::<u64>();
    for c in contracts.iter().filter(|c| c.abandoned.is_none()) {
        let Some(funding) = c.funding else { continue };
        for (_, value) in reserved.iter().filter(|(by, _)| *by == funding) {
            // Its price paid once signed: the rest is her change.
            balance.moving = balance.moving.saturating_sub(*value);
            balance.opening += value.saturating_sub(c.price);
        }
    }
}

/// What each of `contracts` protects, by id, of the notes `held` at its
/// hub (those to come too) at `tip`, none if it is unknown (0): at each
/// hub, of those live that count, the one whose deadline comes first
/// takes up to its cap, then the next of what is left, by id among
/// equals. Those protecting nothing are left out. A way to tell apart
/// what each protects, not notes of its own: together they protect the
/// least of the hub's notes and their caps.
fn covers(
    contracts: &[holder::Contract],
    tip: u32,
    held: &[licash_wallet::Balance],
) -> HashMap<[u8; 32], u64> {
    let mut covers = HashMap::new();
    if tip == 0 {
        return covers;
    }
    for at in held {
        let hub = at.hub.trim_end_matches('/');
        let mut live: Vec<&holder::Contract> = (contracts.iter())
            .filter(|c| c.hub == hub && c.live(tip) && counted(c))
            .collect();
        live.sort_by_key(|c| (c.assert_height, c.id));
        let mut left = at.available + at.pending;
        for c in live {
            let share = left.min(u64::from(c.cap));
            if share > 0 {
                covers.insert(c.id, share);
            }
            left -= share;
        }
    }
    covers
}

/// Whether `c` counts as protecting notes: a mock one, on regtest, as it
/// stands in for one; a genuine one not until this app recovers with it.
fn counted(c: &holder::Contract) -> bool {
    c.mock
}

fn hex(id: &[u8; 32]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

/// Why a contract whose funding can never confirm failed.
const DEAD: &str = "its funding can never confirm: its coin was spent otherwise";

/// `c` as a line of the history, once its funding is broadcast: it pays
/// its price then, done once in a block.
fn activity(c: &holder::Contract) -> Option<Activity> {
    if c.abandoned.is_some() || c.phase < Phase::Funded {
        return None;
    }
    let state = match c.fate {
        Some(Fate::Live) => State::Done,
        Some(Fate::Dead) => State::Failed,
        _ => State::Pending,
    };
    let detail = match state {
        State::Failed => DEAD.into(),
        _ => c.hub.clone(),
    };
    Some(Activity {
        id: hex(&c.id),
        kind: Kind::Contract,
        state,
        amount: c.price,
        fee: 0,
        detail,
        at: Some(c.created_at),
    })
}

/// What an opening at `phase` waits on, `kept` of the instances it keeps
/// on disk: what follows the last step done.
fn opening_step(phase: Phase, kept: usize) -> OpeningStep {
    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    match phase {
        Phase::Started => OpeningStep::Terms,
        Phase::Termed => OpeningStep::Instances,
        Phase::Committed => OpeningStep::Checking,
        Phase::Opened => OpeningStep::Downloading {
            kept: count(kept.min(holder::KEEPS)),
            keeping: count(holder::KEEPS),
        },
        Phase::Consented => OpeningStep::Signatures,
        Phase::Signing => OpeningStep::Signing,
        Phase::Funded => OpeningStep::Confirming { confirmations: 0 },
    }
}

/// How deep a transaction in block `at`, if any, is at `tip`: 1 in the
/// tip, and at least 1 in a block the wallet saw, its tip behind.
fn confirmations(at: Option<u32>, tip: u32) -> u32 {
    at.map_or(0, |at| tip.saturating_sub(at) + 1)
}

/// Where `c` stands at `tip`.
fn state(c: &holder::Contract, tip: u32) -> ContractState {
    if let Some(reason) = &c.abandoned {
        return ContractState::Failed {
            reason: reason.clone(),
        };
    }
    match c.fate {
        Some(Fate::Dead) => ContractState::Failed {
            reason: DEAD.into(),
        },
        Some(Fate::Live) if tip >= c.close_height => ContractState::Closed,
        Some(Fate::Live) if tip >= c.assert_height => ContractState::Ending,
        Some(Fate::Live) => ContractState::Protecting,
        _ if c.phase < Phase::Funded => ContractState::Opening,
        _ => ContractState::Confirming,
    }
}

fn unavailable(e: anyhow::Error) -> LicashError {
    LicashError::from_any(e, |reason| LicashError::Unavailable { reason })
}

fn failed(e: anyhow::Error) -> LicashError {
    LicashError::from_any(e, |reason| LicashError::Failed { reason })
}

#[cfg(test)]
mod tests {
    use bitcoin::hashes::Hash as _;

    use super::*;

    const HUB: &str = "https://hub.example";

    fn contract(id: u8, phase: Phase, fate: Option<Fate>) -> holder::Contract {
        holder::Contract {
            id: [id; 32],
            hub: HUB.into(),
            phase,
            mock: true,
            deposit: OutPoint::null(),
            value: 60_000,
            price: 23_000,
            cap: 4_096,
            assert_height: 360,
            close_height: 500,
            funding: (phase == Phase::Funded).then(|| Txid::from_byte_array([id; 32])),
            fate,
            abandoned: None,
            created_at: 0,
        }
    }

    fn held(hub: &str, available: u64, pending: u64) -> licash_wallet::Balance {
        licash_wallet::Balance {
            hub: hub.into(),
            available,
            pending,
        }
    }

    #[test]
    fn contracts_stand_as_their_phase_fate_and_the_chain_say() {
        let live = contract(1, Phase::Funded, Some(Fate::Live));
        assert_eq!(state(&live, 359), ContractState::Protecting);
        assert_eq!(state(&live, 360), ContractState::Ending);
        assert_eq!(state(&live, 500), ContractState::Closed);
        let state_of = |phase, fate| state(&contract(2, phase, fate), 300);
        assert_eq!(state_of(Phase::Consented, None), ContractState::Opening);
        assert_eq!(state_of(Phase::Funded, None), ContractState::Confirming);
        assert_eq!(
            state_of(Phase::Funded, Some(Fate::Open)),
            ContractState::Confirming
        );
        let dead = state_of(Phase::Funded, Some(Fate::Dead));
        assert!(matches!(dead, ContractState::Failed { .. }), "{dead:?}");
        let mut refused = contract(3, Phase::Termed, None);
        refused.abandoned = Some("refused".into());
        let reason = "refused".to_owned();
        assert_eq!(state(&refused, 300), ContractState::Failed { reason });
    }

    #[test]
    fn an_opening_waits_on_what_follows_its_last_step() {
        use OpeningStep::*;
        let steps = Phase::ALL.map(|phase| opening_step(phase, 1));
        let downloading = Downloading {
            kept: 1,
            keeping: 4,
        };
        let confirming = Confirming { confirmations: 0 };
        assert_eq!(
            steps,
            [
                Terms,
                Instances,
                Checking,
                downloading,
                Signatures,
                Signing,
                confirming
            ]
        );
        // Never more than it keeps, whatever else is on disk.
        let all = opening_step(Phase::Opened, 9);
        let all_kept = Downloading {
            kept: 4,
            keeping: 4,
        };
        assert_eq!(all, all_kept);
        // Its funding in no block yet, in the tip, deeper.
        assert_eq!(confirmations(None, 300), 0);
        assert_eq!(confirmations(Some(300), 300), 1);
        assert_eq!(confirmations(Some(298), 300), 3);
        assert_eq!(confirmations(Some(301), 300), 1);
    }

    #[test]
    fn contracts_are_in_the_history_once_funded_done_once_confirmed() {
        let opening = contract(1, Phase::Consented, None);
        assert_eq!(activity(&opening), None);
        let mut refused = contract(2, Phase::Funded, None);
        refused.abandoned = Some("refused".into());
        assert_eq!(activity(&refused), None);
        let line = |fate| activity(&contract(3, Phase::Funded, fate)).unwrap();
        let confirming = line(None);
        assert_eq!(
            (confirming.kind, confirming.state, confirming.amount),
            (Kind::Contract, State::Pending, 23_000)
        );
        assert_eq!(
            (confirming.id, confirming.detail),
            (hex(&[3; 32]), HUB.into())
        );
        assert_eq!(line(Some(Fate::Open)).state, State::Pending);
        assert_eq!(line(Some(Fate::Live)).state, State::Done);
        let dead = line(Some(Fate::Dead));
        assert_eq!((dead.state, dead.detail.as_str()), (State::Failed, DEAD));
    }

    #[test]
    fn live_contracts_cover_their_hubs_notes_up_to_their_caps() {
        let contracts = [
            contract(1, Phase::Funded, Some(Fate::Live)),
            contract(2, Phase::Funded, Some(Fate::Live)),
            // Not yet, or no longer, protecting.
            contract(3, Phase::Funded, Some(Fate::Open)),
            contract(4, Phase::Consented, None),
        ];
        let covered = |tip, held: &[licash_wallet::Balance]| {
            let mut balance = Balance::default();
            contracted(&mut balance, &contracts, tip, held, &[]);
            balance.covered
        };
        let other = held("https://other.example", 50_000, 0);
        // Two caps at the hub, its pending notes too, the slash aside.
        assert_eq!(
            covered(300, &[held(HUB, 9_000, 1_000), other.clone()]),
            8_192
        );
        assert_eq!(
            covered(300, &[held("https://hub.example/", 5_000, 1_000)]),
            6_000
        );
        // Past their deadline, or with the chain unknown: none.
        assert_eq!(covered(360, &[held(HUB, 9_000, 0)]), 0);
        assert_eq!(covered(0, &[held(HUB, 9_000, 0)]), 0);
    }

    #[test]
    fn each_contract_covers_what_those_ending_first_leave_up_to_its_cap() {
        let ending = |id, assert_height| holder::Contract {
            assert_height,
            ..contract(id, Phase::Funded, Some(Fate::Live))
        };
        // The latest first by id, the earliest first by deadline.
        let contracts = [ending(1, 400), ending(2, 380), ending(3, 400)];
        let other = holder::Contract {
            hub: "https://other.example".into(),
            ..ending(4, 370)
        };
        let all = [&contracts[..], &[other]].concat();
        let covers = |held: &[licash_wallet::Balance]| {
            let covers = covers(&all, 300, held);
            let of = |id: u8| covers.get(&[id; 32]).copied();
            [of(1), of(2), of(3), of(4)]
        };
        let there = held("https://other.example", 1_000, 0);
        assert_eq!(
            covers(&[held(HUB, 9_000, 1_000), there.clone()]),
            [Some(4_096), Some(4_096), Some(1_808), Some(1_000)]
        );
        assert_eq!(
            covers(&[held(HUB, 5_000, 0), there]),
            [Some(904), Some(4_096), None, Some(1_000)]
        );
        assert_eq!(covers(&[held(HUB, 0, 0)]), [None; 4]);
        // Together, what the hub's notes are covered by.
        let mut balance = Balance::default();
        contracted(&mut balance, &all, 300, &[held(HUB, 5_000, 0)], &[]);
        assert_eq!(balance.covered, 5_000);
    }

    #[test]
    fn genuine_contracts_cover_nothing_until_this_app_recovers_with_them() {
        let genuine = holder::Contract {
            mock: false,
            ..contract(1, Phase::Funded, Some(Fate::Live))
        };
        let mock = contract(2, Phase::Funded, Some(Fate::Live));
        let covered = |contracts: &[holder::Contract]| {
            let mut balance = Balance::default();
            contracted(&mut balance, contracts, 300, &[held(HUB, 9_000, 0)], &[]);
            balance.covered
        };
        assert_eq!(covered(std::slice::from_ref(&genuine)), 0);
        assert_eq!(covered(&[genuine, mock]), 4_096);
    }

    #[test]
    fn a_coin_paying_for_a_contract_shows_as_its_change_to_come() {
        let signed = contract(1, Phase::Funded, None);
        let f = signed.funding.unwrap();
        let other = Txid::from_byte_array([9; 32]);
        let mut balance = Balance {
            moving: 60_000 + 10_000,
            ..Balance::default()
        };
        let reserved = [(f, 60_000), (other, 10_000)];
        contracted(&mut balance, &[signed], 300, &[], &reserved);
        assert_eq!((balance.moving, balance.opening), (10_000, 37_000));
    }

    fn range() -> licash_api::ContractRange {
        licash_api::ContractRange {
            min_cap: 4_096,
            max_cap: 100_000_000,
            min_blocks: 432,
            max_blocks: 26_280,
            per_coin: 2,
            base: 23_000,
            ppm_year: 10_000,
        }
    }

    #[test]
    fn a_contract_chosen_is_at_first_half_again_the_largest_coin_for_half_a_year() {
        // The 10 M sat coin: 15 M sat, 10 M once moved and 5 M to receive.
        assert_eq!(suggested(&range(), Some(10_000_000)), (15_000_000, 26_280));
        // Within the hub's range and twice the coin.
        assert_eq!(suggested(&range(), Some(90_000_000)), (100_000_000, 26_280));
        assert_eq!(suggested(&range(), Some(2_500)), (4_096, 26_280));
        // No coin, or none large enough: the least.
        assert_eq!(suggested(&range(), None), (4_096, 26_280));
        assert_eq!(suggested(&range(), Some(2_000)), (4_096, 26_280));
        // Half a year, or as long as the hub lets.
        let short = licash_api::ContractRange {
            max_blocks: 4_000,
            ..range()
        };
        assert_eq!(suggested(&short, Some(10_000_000)).1, 4_000);
    }

    fn coin(txid: Txid, vout: u32, spending: Option<Txid>) -> licash_wallet::SilentCoin {
        licash_wallet::SilentCoin {
            outpoint: OutPoint { txid, vout },
            value: 9_900_000,
            height: 300,
            change: false,
            spending,
        }
    }

    #[test]
    fn a_contracts_change_moves_once_its_funding_confirms_and_never_twice() {
        let deposit = OutPoint {
            txid: Txid::from_byte_array([7; 32]),
            vout: 0,
        };
        let paid = |phase, fate| holder::Contract {
            deposit,
            ..contract(1, phase, fate)
        };
        let f = Txid::from_byte_array([1; 32]);
        let change_coin = coin(f, 1, None);
        // Its opening not started, or given up: nothing.
        assert_eq!(change(&[], &[], &deposit), Change::None);
        let given_up = holder::Contract {
            abandoned: Some("idle".into()),
            ..paid(Phase::Funded, None)
        };
        assert_eq!(change(&[given_up], &[], &deposit), Change::None);
        let dead = paid(Phase::Funded, Some(Fate::Dead));
        assert_eq!(change(&[dead], &[], &deposit), Change::None);
        // Being opened, or signed and not seen confirmed: later.
        assert_eq!(
            change(&[paid(Phase::Opened, None)], &[], &deposit),
            Change::Wait
        );
        let signed = paid(Phase::Funded, Some(Fate::Open));
        assert_eq!(
            change(std::slice::from_ref(&signed), &[], &deposit),
            Change::Wait
        );
        // Its change seen: moved, to its hub.
        let coins = [
            coin(Txid::from_byte_array([2; 32]), 0, None),
            change_coin.clone(),
        ];
        assert_eq!(
            change(std::slice::from_ref(&signed), &coins, &deposit),
            Change::Move(change_coin.outpoint, HUB)
        );
        // Being spent already, by the move as it may be: not again.
        let spent = [coin(f, 1, Some(Txid::from_byte_array([3; 32])))];
        assert_eq!(
            change(std::slice::from_ref(&signed), &spent, &deposit),
            Change::None
        );
        // Buried with no change of the wallet's: none ever comes.
        let live = paid(Phase::Funded, Some(Fate::Live));
        assert_eq!(change(&[live], &[], &deposit), Change::None);
    }

    #[test]
    fn a_change_too_small_to_move_is_not_tried_again_one_unreachable_is() {
        let small = LicashError::Insufficient {
            needed: 10_000,
            available: 9_900,
        };
        assert!(!retried(&small));
        assert!(!retried(&LicashError::invalid(
            "no hub trusted takes coins"
        )));
        let down = LicashError::Unavailable {
            reason: "broadcasting".into(),
        };
        assert!(retried(&down));
        assert!(retried(&LicashError::NeedsTor));
        assert!(retried(&LicashError::Failed {
            reason: "disk".into()
        }));
    }

    #[test]
    fn the_changes_to_move_are_kept_whole_and_none_without_them() {
        let dir = tempfile::tempdir().unwrap();
        assert!(changes(dir.path()).unwrap().is_empty());
        let a = OutPoint {
            txid: Txid::from_byte_array([1; 32]),
            vout: 0,
        };
        let b = OutPoint { vout: 3, ..a };
        set_changes(dir.path(), &[a, b]).unwrap();
        assert_eq!(changes(dir.path()).unwrap(), [a, b]);
        // A line that does not read moves nothing; the rest stand.
        let path = dir.path().join(CHANGES);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{text}not a coin\n")).unwrap();
        assert_eq!(changes(dir.path()).unwrap(), [a, b]);
        set_changes(dir.path(), &[]).unwrap();
        assert!(!path.exists());
        assert!(changes(dir.path()).unwrap().is_empty());
        set_changes(dir.path(), &[]).unwrap();
    }

    #[test]
    fn the_app_prices_a_contract_chosen_as_its_hub_does() {
        let range = crate::ContractRange::from(range());
        assert_eq!(range.api(), super::tests::range());
        // 15 M sat for half a year at 1% a year, over the base.
        assert_eq!(crate::contract_price(range, 15_000_000, 26_280), 98_000);
        assert_eq!(crate::contract_price(range, 4_096, 1_000), 23_000);
        // A cap past what a hub could offer: priced as the most.
        let most = crate::contract_price(range, u64::from(u32::MAX), 26_280);
        assert_eq!(crate::contract_price(range, u64::MAX, 26_280), most);
        assert_eq!(crate::contract_max_cap(range, 10_000_000), Some(20_000_000));
        assert_eq!(crate::contract_max_cap(range, 2_047), None);
    }
}
