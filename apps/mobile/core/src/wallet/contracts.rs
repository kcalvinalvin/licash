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
    chain::Chain, Activity, Balance, ChainSource, Contract, ContractQuote, ContractState,
    DepositCoin, Details, Kind, LicashError, OpeningStep, State, Transfer,
};

/// How large an instance of a contract's opening is, about: her wallet
/// uploads each the hub keeps.
const INSTANCE: u64 = 25_100_000;

/// What opening a contract sends, in bytes, about: the `kept` instances
/// the hub keeps of its quote's.
fn upload(kept: u16) -> u64 {
    u64::from(kept) * INSTANCE
}

/// Blocks a contract chosen lasts at first: half a year.
const HALF_YEAR: u32 = 26_280;

/// Recovery contracts.
#[uniffi::export]
impl Wallet {
    pub fn contract_inspection(
        &self,
        id: String,
    ) -> Result<crate::ContractInspection, LicashError> {
        let mut bytes = [0; 32];
        hex::decode_to_slice(id, &mut bytes)
            .map_err(|_| LicashError::invalid("not a contract id"))?;
        let info = holder::inspect(&self.wallet, &self.dir, &bytes, self.tip()).map_err(failed)?;
        Ok(crate::ContractInspection {
            address: info.address,
            locked: info.locked,
            payout_pool: info.payout_pool,
            holder_reserve: info.holder_reserve,
            hub_reserve: info.hub_reserve,
            claim_limit: info.claim_limit,
            note_limit: info.note_limit,
            denominations: info.denominations,
            keyset: info.keyset,
            graph_version: info.graph_version,
            program: info.program,
        })
    }

    /// What opening a contract at `hub`, a hub trusted, costs now, as it
    /// offers, and the coin on Bitcoin that would pay it.
    pub fn contract_quote(&self, hub: String) -> Result<ContractQuote, LicashError> {
        let (hub, terms) = self.contracting_hub(&hub)?;
        let trust = licash_contracts::trust(self.wallet.network());
        let trust = trust.map_err(|e| LicashError::invalid(format!("{e:#}")))?;
        let offer = holder::offer(&self.wallet, &hub, None, 0);
        let offer = self.runtime.block_on(offer).map_err(unavailable)?;
        let price = offer.price();
        let coin = holder::coin(&self.wallet, &self.dir, price).map_err(failed)?;
        let coins = holder::coins(&self.wallet, &self.dir).map_err(failed)?;
        let blocks = offer.bounds.lock;
        let after_assert = holder::after_assert(&offer.quote);
        let largest = coins.first().map(|c| c.value);
        let (denominations, funded_notes) = holder::funding_notes(&offer.quote);
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
            download: upload(offer.quote.recipe.kept),
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
            dust: offer.quote.graph.dust,
            denominations,
            funded_notes: u32::try_from(funded_notes).unwrap_or(u32::MAX),
        })
    }

    /// The contract to offer the wallet, if any: for its largest coin on
    /// Bitcoin, at the trusted hub letting it choose terms that protects
    /// the most for the least (`best`), none at a hub it has one live or
    /// being opened. Its quote, at first of the suggested terms; opened
    /// only once she accepts them ([`Wallet::open_chosen_contract`]). None
    /// if no hub offers one, no coin pays one for a tenth of it at most
    /// (`SHARE`), or no hub answers.
    pub fn contract_offer(&self) -> Result<Option<ContractQuote>, LicashError> {
        let all = holder::list(&self.dir).map_err(failed)?;
        let tip = self.tip();
        let busy: Vec<&str> = all
            .iter()
            .filter(|c| {
                matches!(
                    state(c, tip),
                    ContractState::Opening | ContractState::Confirming | ContractState::Protecting
                )
            })
            .map(|c| c.hub.trim_end_matches('/'))
            .collect();
        let coins = holder::coins(&self.wallet, &self.dir).map_err(failed)?;
        let Some(coin) = coins.first().map(|c| c.value) else {
            return Ok(None);
        };
        let hubs: Vec<Offering> = self
            .wallet
            .hubs()?
            .into_iter()
            .filter(|(url, _)| !busy.contains(&url.trim_end_matches('/')))
            .filter_map(|(url, info)| Some((url, info.contracts?.range?)))
            .filter(|(_, range)| check_range(range).is_ok())
            .collect();
        for hub in best(&hubs, coin) {
            match self.contract_quote(hub.to_owned()) {
                Ok(quote) => return Ok(Some(quote)),
                Err(e) => tracing::info!(hub, "no contract offered there now: {e}"),
            }
        }
        Ok(None)
    }

    /// Open a contract at `hub`, a hub trusted, closing `blocks` from now
    /// (by default, as late as it allows), for at most `max_price` (by
    /// default, what its info says), broadcasting its funding through
    /// `chain`: the contract, confirming once its funding is signed.
    /// Paid from the smallest coin on Bitcoin that pays it, its change
    /// back to the wallet on Bitcoin. Takes minutes: its downloads are
    /// large. One at a time; under way, [`Wallet::resume_contracts`]
    /// carries it on.
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
            funded: 0,
        };
        let _contracting = self
            .contracting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.opened(chain, &hub, ask)
    }

    /// Open a contract at `hub`, a hub trusted that lets the wallet
    /// choose (its terms' range), protecting up to `cap` sat and closing
    /// `blocks` from now, `funded` sat of it funded by her, for at most
    /// `max_price`, paid from `coin` (`txid:vout`; by default the smallest
    /// large enough), as [`Wallet::open_contract`]. What she funds is paid
    /// from the coin beside the price, the hub's interest charged on the
    /// rest only, and issued her as notes at the hub once its funding is
    /// buried ([`crate::contract_fundable`] says what amounts can be).
    #[allow(clippy::too_many_arguments)]
    pub fn open_chosen_contract(
        &self,
        chain: Arc<dyn ChainSource>,
        hub: String,
        cap: u64,
        blocks: u32,
        funded: u64,
        max_price: u64,
        coin: Option<String>,
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
                let least = holder::least(Some(range), cap, max_price.saturating_add(funded));
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
            funded,
        };
        self.opened(chain, &hub, ask)
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
        // has deadlines of its own. Its fees paid out of notes first, if
        // its coins pay too little.
        self.fund_recoveries(&chain);
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
        let opened = holder::open_with(&self.wallet, &chain, &self.dir, hub, ask);
        let opened = self.runtime.block_on(opened).map_err(unavailable)?;
        // Protecting nothing before its funding is buried.
        Ok(self.contract(&opened, 0))
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
            Some(terms) => {
                if let Some(range) = &terms.range {
                    check_range(range)?;
                }
                Ok((hub.to_owned(), terms))
            }
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
        let covers = holder::coverage(&self.wallet, &self.dir, all, self.tip()).map_err(failed)?;
        let covered = |c: &holder::Contract| covers.get(&c.id).copied().unwrap_or(0);
        Ok(all.iter().map(|c| self.contract(c, covered(c))).collect())
    }

    fn contract(&self, c: &holder::Contract, covers: u64) -> Contract {
        let (tip, state) = (self.tip(), state(c, self.tip()));
        let step = match state {
            ContractState::Opening => Some(match (c.phase, holder::stepping(&c.id)) {
                // Its terms got: working out what it commits to on them.
                (Phase::Started, Some(took)) => OpeningStep::Preparing {
                    seconds: took.as_secs(),
                },
                (phase, _) => opening_step(phase),
            }),
            ContractState::Confirming => {
                // As deep as the wallet saw her coin's spend: F, which
                // spends it.
                let spent_at = self.wallet.silent_spent_at(&c.deposit).ok().flatten();
                let confirmations = confirmations(spent_at, tip);
                Some(OpeningStep::Confirming { confirmations })
            }
            _ => None,
        };
        let transfer = (matches!(state, ContractState::Opening))
            .then(|| holder::traffic(&c.id))
            .flatten()
            .map(|t| Transfer {
                bytes: t.bytes,
                seconds: t.since.elapsed().as_secs(),
                // As a hub keeps on signet or mainnet, until it builds one.
                expected: t.expected.unwrap_or(upload(10)).max(t.bytes),
            });
        Contract {
            id: hex(&c.id),
            hub: c.hub.clone(),
            state,
            step,
            transfer,
            mock: c.mock,
            cap: u64::from(c.cap),
            covers,
            price: c.price,
            funded: c.funded,
            funded_received: c.funded_received,
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
        _held: &[licash_wallet::Balance],
        reserved: &[(Txid, u64)],
    ) -> Result<(), LicashError> {
        let all = holder::list(&self.dir).map_err(failed)?;
        if !all.is_empty() {
            let covered =
                holder::coverage(&self.wallet, &self.dir, &all, self.tip()).map_err(failed)?;
            contracted(balance, &all, covered.values().sum(), reserved);
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
    /// their funding yet: no move is to take them. Those older than an
    /// opening may take ([`holder::OPENS_WITHIN`]), given up, keep none.
    pub(super) fn opening_coins(&self) -> Result<Vec<OutPoint>, LicashError> {
        let all = holder::list(&self.dir).map_err(failed)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let opening = all.iter().filter(|c| {
            c.abandoned.is_none()
                && c.phase < Phase::Funded
                && now - c.created_at < holder::OPENS_WITHIN
        });
        Ok(opening.map(|c| c.deposit).collect())
    }
}

/// Refuse malformed advertised ranges before choosing or pricing terms.
fn check_range(range: &licash_api::ContractRange) -> Result<(), LicashError> {
    if range.min_blocks > range.max_blocks || range.min_cap > range.max_cap {
        return Err(LicashError::invalid(
            "the hub offers an inverted contract range",
        ));
    }
    Ok(())
}

/// What a contract chosen within the checked `range` is at first: half
/// again `coin`, up to a size, over half a year, within the advertised
/// bounds.
fn suggested(range: &licash_api::ContractRange, coin: Option<u64>) -> (u32, u32) {
    let blocks = HALF_YEAR.clamp(range.min_blocks, range.max_blocks);
    let most = coin.and_then(|c| range.max_cap_for(c).map(|most| (c, most)));
    let cap = match most {
        Some((coin, most)) => {
            let half_again = coin.saturating_add(coin / 2);
            let up = licash_api::sizes().find(|&s| u64::from(s) >= half_again);
            up.unwrap_or(most).clamp(range.min_cap, most)
        }
        None => range.min_cap,
    };
    (cap, blocks)
}

/// The most of a coin a contract offered unasked costs: a tenth.
const SHARE: u64 = 10;

/// A hub letting the wallet choose terms: its URL and its range.
type Offering = (String, licash_api::ContractRange);

/// Of `hubs`, those to offer a contract at, paid from a `coin` of so many
/// sat, of the suggested terms ([`suggested`]), none of it funded by her:
/// the one protecting the most first, then the cheapest. None whose price
/// is past a [`SHARE`] of the coin, or that the coin does not pay.
fn best(hubs: &[Offering], coin: u64) -> Vec<&str> {
    let mut offers: Vec<(&str, u32, u64)> = hubs
        .iter()
        .filter_map(|(hub, range)| {
            let (cap, blocks) = suggested(range, Some(coin));
            range.max_cap_for(coin)?;
            let price = range.price(cap, blocks, 0);
            let pays = coin >= holder::least(Some(*range), cap, price);
            (pays && price <= coin / SHARE).then_some((hub.as_str(), cap, price))
        })
        .collect();
    offers.sort_by_key(|&(_, cap, price)| (std::cmp::Reverse(cap), price));
    offers.into_iter().map(|(hub, ..)| hub).collect()
}

/// Of `balance`, the precisely claimable `covered`, and what contracts
/// being opened hold of the coins `reserved`
/// for transactions, by their ids: its `covered`, and its `opening` rather
/// than `moving`.
fn contracted(
    balance: &mut Balance,
    contracts: &[holder::Contract],
    covered: u64,
    reserved: &[(Txid, u64)],
) {
    balance.covered += covered;
    for c in contracts.iter().filter(|c| c.abandoned.is_none()) {
        let Some(funding) = c.funding else { continue };
        for (_, value) in reserved.iter().filter(|(by, _)| *by == funding) {
            // Its price paid once signed: the rest is her change.
            balance.moving = balance.moving.saturating_sub(*value);
            balance.opening += value.saturating_sub(c.price);
        }
    }
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

/// What an opening at `phase` waits on: what follows the last step done.
fn opening_step(phase: Phase) -> OpeningStep {
    match phase {
        Phase::Started => OpeningStep::Terms,
        Phase::Committed => OpeningStep::Choosing,
        Phase::Chosen => OpeningStep::Uploading,
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
    if c.folded.is_some() {
        return ContractState::Closed;
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
            since: 200,
            funding: (phase == Phase::Funded).then(|| Txid::from_byte_array([id; 32])),
            fate,
            abandoned: None,
            created_at: 0,
            folded: None,
            funded: 0,
            funded_received: false,
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
        let mut refused = contract(3, Phase::Committed, None);
        refused.abandoned = Some("refused".into());
        let reason = "refused".to_owned();
        assert_eq!(state(&refused, 300), ContractState::Failed { reason });
    }

    #[test]
    fn an_opening_waits_on_what_follows_its_last_step() {
        use OpeningStep::*;
        let steps = Phase::ALL.map(opening_step);
        let confirming = Confirming { confirmations: 0 };
        assert_eq!(
            steps,
            [Terms, Choosing, Uploading, Signatures, Signing, confirming]
        );
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
    fn balance_uses_claimable_coverage_instead_of_nominal_caps() {
        let contracts = [
            contract(1, Phase::Funded, Some(Fate::Live)),
            contract(2, Phase::Funded, Some(Fate::Live)),
            // Not yet, or no longer, protecting.
            contract(3, Phase::Funded, Some(Fate::Open)),
            contract(4, Phase::Consented, None),
        ];
        let covered = |claimable| {
            let mut balance = Balance::default();
            contracted(&mut balance, &contracts, claimable, &[]);
            balance.covered
        };
        assert_eq!(covered(68), 68);
        assert_eq!(covered(0), 0);
    }

    #[test]
    fn aggregate_coverage_preserves_opening_coin_accounting() {
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
        let mut balance = Balance {
            covered: 2,
            ..Balance::default()
        };
        contracted(&mut balance, &all, 68, &[]);
        assert_eq!(balance.covered, 70);
        assert_eq!(balance.opening, 0);
    }

    #[test]
    fn genuine_claimable_coverage_is_not_gated_on_phone_proving() {
        let genuine = holder::Contract {
            mock: false,
            ..contract(1, Phase::Funded, Some(Fate::Live))
        };
        let mock = contract(2, Phase::Funded, Some(Fate::Live));
        let covered = |contracts: &[holder::Contract]| {
            let mut balance = Balance::default();
            contracted(&mut balance, contracts, 68, &[]);
            balance.covered
        };
        assert_eq!(covered(std::slice::from_ref(&genuine)), 68);
        assert_eq!(covered(&[genuine, mock]), 68);
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
        contracted(&mut balance, &[signed], 0, &reserved);
        assert_eq!((balance.moving, balance.opening), (10_000, 37_000));
    }

    fn range() -> licash_api::ContractRange {
        licash_api::ContractRange {
            min_cap: 131_071,
            max_cap: 67_108_863,
            min_blocks: 432,
            max_blocks: 26_280,
            per_coin: 2,
            reserve: 20_000,
            fee: 3_000,
            ppm_year: 10_000,
            ppm_year_funded: 0,
        }
    }

    #[test]
    fn a_contract_chosen_is_at_first_half_again_the_largest_coin_for_half_a_year() {
        // The 10 M sat coin: 15 M sat, up to a size.
        assert_eq!(suggested(&range(), Some(10_000_000)), (16_777_215, 26_280));
        assert_eq!(suggested(&range(), Some(5_000_000)), (8_388_607, 26_280));
        // Within the hub's range and twice the coin.
        assert_eq!(suggested(&range(), Some(90_000_000)), (67_108_863, 26_280));
        assert_eq!(suggested(&range(), Some(100_000)), (131_071, 26_280));
        // No coin, or none large enough: the least.
        assert_eq!(suggested(&range(), None), (131_071, 26_280));
        assert_eq!(suggested(&range(), Some(2_000)), (131_071, 26_280));
        // Half a year, or as long as the hub lets.
        let short = licash_api::ContractRange {
            max_blocks: 4_000,
            ..range()
        };
        assert_eq!(suggested(&short, Some(10_000_000)).1, 4_000);
    }

    #[test]
    fn a_contract_is_offered_where_it_protects_the_most_for_the_least() {
        let hub = |name: &str, range| (format!("https://{name}"), range);
        let dear = licash_api::ContractRange {
            ppm_year: 20_000,
            ..range()
        };
        let small = licash_api::ContractRange {
            max_cap: 1_000_000,
            ..range()
        };
        let hubs = [
            hub("small", small),
            hub("dear", dear),
            hub("cheap", range()),
        ];
        // The 10 M sat coin: 16 777 215 sat protected where it can be,
        // for 106 886 sat at the cheaper; 524 287 where 1 M sat at most is.
        assert_eq!(
            best(&hubs, 10_000_000),
            ["https://cheap", "https://dear", "https://small"]
        );
        // Costing past a tenth of the coin: none offered.
        assert!(best(&hubs, 200_000).is_empty());
        assert_eq!(best(&hubs[2..], 300_000), ["https://cheap"]);
        // A coin paying none: none.
        assert!(best(&hubs, 2_000).is_empty());
        assert!(best(&[], 10_000_000).is_empty());
    }

    #[test]
    fn a_contract_chosen_starts_at_a_size_its_coin_pays() {
        let range = crate::ContractRange::from(range());
        let start = |coin, cap| crate::payable_size(range, coin, cap, 26_280);
        // The 10 M sat coin pays the size above half again itself.
        assert_eq!(start(10_000_000, 16_777_215), Some(16_777_215));
        // Asked between sizes: the one below.
        assert_eq!(start(10_000_000, 20_000_000), Some(16_777_215));
        // Twice the coin at most: the largest the coin pays.
        assert_eq!(start(10_000_000, 33_554_431), Some(16_777_215));
        // A coin of 70 k sat pays the least, 131 071 sat, the rest back.
        assert_eq!(start(70_000, 262_143), Some(131_071));
        // Too small a coin for the least: none.
        assert_eq!(start(23_000, 131_071), None);
    }

    #[test]
    fn she_funds_what_her_coin_pays_beside_the_price_rounded_to_notes_a_claim_names() {
        let range = crate::ContractRange::from(range());
        let wide: Vec<u64> = (0..32).map(|i| 1 << i).collect();
        let most =
            |cap, coin| crate::contract_most_funded(range, cap, 26_280, coin, wide.clone(), 32);
        let funded = most(131_071, 100_000);
        let pays = |f| f + crate::contract_price(range, 131_071, 26_280, f);
        assert!(
            pays(funded) <= 100_000 && pays(funded + 1) > 100_000,
            "{funded}"
        );
        assert_eq!(
            crate::contract_least_coin(range, 131_071, 26_280, funded),
            pays(funded)
        );
        // All of it hers at most; none of a coin short of the price, or
        // too small for so large a cap.
        assert_eq!(most(131_071, 1_000_000), 131_071);
        assert_eq!(most(131_071, 20_000), 0);
        assert_eq!(most(1_048_575, 300_000), 0);
        assert_eq!(
            crate::contract_least_coin(range, 1_048_575, 26_280, 0),
            524_288
        );
        // A hub of notes 2 048 at most: 32 of them at most.
        let twos: Vec<u64> = (0..12).map(|i| 1 << i).collect();
        assert_eq!(crate::contract_fundable(twos.clone(), 32, 65_535), 64_512);
        let rounded = crate::contract_most_funded(range, 131_071, 26_280, 100_000, twos, 32);
        assert_eq!(rounded, 65_536);
    }

    #[test]
    fn inverted_contract_ranges_are_refused_before_suggesting_terms() {
        assert!(check_range(&range()).is_ok());
        let mut inverted = range();
        inverted.min_blocks = inverted.max_blocks + 1;
        assert!(check_range(&inverted).is_err());
        let mut inverted = range();
        inverted.min_cap = inverted.max_cap + 1;
        assert!(check_range(&inverted).is_err());
        let mut single = range();
        single.min_blocks = single.max_blocks;
        single.min_cap = single.max_cap;
        assert!(check_range(&single).is_ok());
        assert_eq!(
            suggested(&single, None),
            (single.min_cap, single.min_blocks)
        );
    }

    #[test]
    fn the_app_prices_a_contract_chosen_as_its_hub_does() {
        let range = crate::ContractRange::from(range());
        assert_eq!(range.api(), super::tests::range());
        // 16 777 215 sat for half a year at 1% a year, over the reserve
        // and fee.
        assert_eq!(crate::contract_price(range, 16_777_215, 26_280, 0), 106_886);
        assert_eq!(crate::contract_price(range, 131_071, 1_000, 0), 23_024);
        // Half of it hers: interest on the hub's half only.
        let fee = crate::contract_fee(range, 16_777_215, 26_280, 8_388_607);
        assert_eq!(
            (fee.opening, fee.interest, fee.interest_funded),
            (3_000, 41_943, 0)
        );
        assert_eq!(
            crate::contract_price(range, 16_777_215, 26_280, 8_388_607),
            20_000 + 3_000 + 41_943
        );
        assert_eq!(crate::contract_sizes(range).len(), 10);
        // A cap past what a hub could offer: priced as the most.
        let most = crate::contract_price(range, u64::from(u32::MAX), 26_280, 0);
        assert_eq!(crate::contract_price(range, u64::MAX, 26_280, 0), most);
        assert_eq!(crate::contract_max_cap(range, 10_000_000), Some(16_777_215));
        assert_eq!(crate::contract_max_cap(range, 65_535), None);
    }
}
