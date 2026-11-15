//! Recovery contracts with the hubs trusted, opened from a coin of the
//! wallet's on Bitcoin, carried on and watched as the chain moves: each
//! protects the wallet's notes at its hub, up to its cap, until its last
//! block to recover in; its collateral returns to the hub at its close.
//! There is no closing one sooner: the notes it protects are cashed out
//! instead, and it lapses.
//!
//! Kept in the wallet's directory, `contracts.sqlite` and `contracts/`,
//! made only once a first contract is asked for.

use std::sync::{atomic::Ordering, Arc, PoisonError};

use bitcoin::{OutPoint, Txid};
use licash_contracts::holder::{self, Ask, Fate, Phase};

use super::Wallet;
use crate::{
    chain::Chain, Activity, Balance, ChainSource, Contract, ContractQuote, ContractState, Details,
    Kind, LicashError, State,
};

/// What opening a contract downloads and keeps, in bytes, about: the
/// hub's instances its holder keeps, four of 25 MB.
const DOWNLOAD: u64 = 100_100_000;

/// How long a contract being opened keeps its coin from moving into
/// Licash, in seconds: the hub gives up on one idle an hour, and a coin
/// kept longer would never move.
const OPENING: i64 = 86_400;

/// Recovery contracts.
#[uniffi::export]
impl Wallet {
    /// What opening a contract at `hub`, a hub trusted, costs now, as it
    /// offers, and the coin on Bitcoin that would pay it.
    pub fn contract_quote(&self, hub: String) -> Result<ContractQuote, LicashError> {
        let (hub, _) = self.contracting_hub(&hub)?;
        let trust = licash_contracts::trust(self.wallet.network());
        let trust =
            trust.map_err(|_| LicashError::invalid("contracts open on regtest only, for now"))?;
        let offer = holder::offer(&self.wallet, &hub);
        let offer = self.runtime.block_on(offer).map_err(unavailable)?;
        let price = offer.price();
        let coin = holder::coin(&self.wallet, &self.dir, price).map_err(failed)?;
        let blocks = offer.bounds.lock;
        Ok(ContractQuote {
            hub,
            price,
            cap: u64::from(offer.quote.cap),
            blocks,
            protects_for: blocks.saturating_sub(holder::after_assert(&offer.quote)),
            mock: trust.mock,
            coin: coin.map(|c| c.value),
            download: DOWNLOAD,
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
        };
        let _contracting = self
            .contracting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let chain = Chain(chain, self.wallet.network());
        let tip = self.runtime.block_on(licash_wallet::Blocks::height(&chain));
        self.saw(tip.map_err(|reason| LicashError::Unavailable { reason })?);
        let opened = holder::open(&self.wallet, &chain, &self.dir, &hub, ask);
        let opened = self.runtime.block_on(opened).map_err(unavailable)?;
        Ok(self.contract(&opened))
    }

    /// The contracts, the newest first, as last seen.
    pub fn contracts(&self) -> Result<Vec<Contract>, LicashError> {
        let all = holder::list(&self.dir).map_err(failed)?;
        Ok(all.iter().map(|c| self.contract(c)).collect())
    }

    /// Carry on with the contracts being opened, and see where the funding
    /// of each signed stands on `chain`: every contract, the newest first.
    /// While one is being opened, as last seen.
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
        let all = self.runtime.block_on(all).map_err(unavailable)?;
        Ok(all.iter().map(|c| self.contract(c)).collect())
    }
}

impl Wallet {
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

    fn contract(&self, c: &holder::Contract) -> Contract {
        Contract {
            id: hex(&c.id),
            hub: c.hub.clone(),
            state: state(c, self.tip()),
            mock: c.mock,
            cap: u64::from(c.cap),
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

/// Of `balance`, `held` at each hub, what `contracts` protect at `tip`
/// (none if it is unknown, 0), and what those being opened hold of the
/// coins `reserved` for transactions, by their ids: its `covered`, and
/// its `opening` rather than `moving`.
fn contracted(
    balance: &mut Balance,
    contracts: &[holder::Contract],
    tip: u32,
    held: &[licash_wallet::Balance],
    reserved: &[(Txid, u64)],
) {
    for at in held {
        let hub = at.hub.trim_end_matches('/');
        let live = contracts
            .iter()
            .filter(|c| c.hub == hub && tip > 0 && c.live(tip) && counted(c));
        let cap: u64 = live.map(|c| u64::from(c.cap)).sum();
        balance.covered += cap.min(at.available + at.pending);
    }
    for c in contracts.iter().filter(|c| c.abandoned.is_none()) {
        let Some(funding) = c.funding else { continue };
        for (_, value) in reserved.iter().filter(|(by, _)| *by == funding) {
            // Its price paid once signed: the rest is her change.
            balance.moving = balance.moving.saturating_sub(*value);
            balance.opening += value.saturating_sub(c.price);
        }
    }
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
}
